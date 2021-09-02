use super::{
  indexed::{IndexSetIteratorExt, IndexedDomain, ToIndex},
  indexed_impls::{PlaceDomain, PlaceIndex, PlaceSet},
  utils::{self, elapsed, PlaceRelation},
};
use log::debug;
use polonius_engine::FactTypes;
use rustc_data_structures::{
  fx::{FxHashMap as HashMap, FxHashSet as HashSet, FxIndexMap as IndexMap},
  graph::{scc::Sccs, vec_graph::VecGraph},
};
use rustc_index::{
  bit_set::{BitSet, SparseBitMatrix},
  vec::IndexVec,
};
use rustc_middle::{
  mir::{visit::Visitor, *},
  ty::{RegionKind, RegionVid, TyCtxt},
};
use rustc_mir::consumers::RustcFacts;
use std::{cell::RefCell, rc::Rc, time::Instant};

struct GatherBorrows<'tcx> {
  borrows: Vec<(RegionVid, BorrowKind, Place<'tcx>)>,
}

impl Visitor<'tcx> for GatherBorrows<'tcx> {
  fn visit_assign(&mut self, _place: &Place<'tcx>, rvalue: &Rvalue<'tcx>, _location: Location) {
    if let Rvalue::Ref(region, kind, borrowed_place) = *rvalue {
      let region_vid = match region {
        RegionKind::ReVar(region_vid) => *region_vid,
        _ => unreachable!(),
      };
      self.borrows.push((region_vid, kind, borrowed_place));
    }
  }
}

#[derive(Debug, Clone)]
pub struct Conflicts<'tcx> {
  pub subs: PlaceSet<'tcx>,
  pub supers: PlaceSet<'tcx>,
  pub single_pointee: bool,
}

impl Conflicts<'tcx> {
  pub fn iter<'a>(&'a self) -> impl Iterator<Item = PlaceIndex> + 'a {
    self.subs.indices().chain(self.supers.indices())
  }
}

pub struct Aliases<'tcx> {
  loans: IndexVec<RegionVid, PlaceSet<'tcx>>,
  loan_locals: SparseBitMatrix<Local, RegionVid>,
  regions: IndexMap<PlaceIndex, RegionVid>,
  loan_cache: RefCell<IndexMap<PlaceIndex, Conflicts<'tcx>>>,
  tcx: TyCtxt<'tcx>,

  pub place_domain: Rc<PlaceDomain<'tcx>>,
}

pub type Point = <RustcFacts as FactTypes>::Point;
pub type OutlivesConstraint = (RegionVid, RegionVid, Point);

rustc_index::newtype_index! {
  pub struct ConstraintSccIndex {
      DEBUG_FORMAT = "cs{}"
  }
}

impl Aliases<'tcx> {
  fn compute_region_ancestors(
    sccs: &Sccs<RegionVid, ConstraintSccIndex>,
    regions_in_scc: &IndexVec<ConstraintSccIndex, BitSet<RegionVid>>,
    node: ConstraintSccIndex,
  ) -> HashMap<RegionVid, BitSet<ConstraintSccIndex>> {
    let new_set = || BitSet::new_empty(sccs.num_sccs());
    let set_merge = |s1: &mut BitSet<ConstraintSccIndex>, s2: BitSet<ConstraintSccIndex>| {
      s1.union(&s2);
    };

    let mut initial_set = new_set();
    initial_set.insert(node);

    let mut initial_map = HashMap::default();
    for r in regions_in_scc[node].iter() {
      initial_map.insert(r, initial_set.clone());
    }

    sccs
      .successors(node)
      .iter()
      .map(|child| {
        let in_child = regions_in_scc[*child]
          .iter()
          .map(|r| (r, initial_set.clone()))
          .collect::<HashMap<_, _>>();

        let grandchildren = Self::compute_region_ancestors(sccs, regions_in_scc, *child)
          .into_iter()
          .map(|(region, mut parents)| {
            parents.insert(node);
            (region, parents)
          })
          .collect::<HashMap<_, _>>();

        utils::hashmap_merge(in_child, grandchildren, set_merge)
      })
      .fold(initial_map, |h1, h2| {
        utils::hashmap_merge(h1, h2, set_merge)
      })
  }

  pub fn aliases(&self, place: impl ToIndex<Place<'tcx>>) -> PlaceSet<'tcx> {
    let place_index = place.to_index(&self.place_domain);
    let place = *self.place_domain.value(place_index);

    macro_rules! early_return {
      () => {
        let mut set = PlaceSet::new(self.place_domain.clone());
        set.insert(place_index);
        return set;
      };
    }

    let (ptr, projection_past_deref) = match utils::split_deref(place, self.tcx) {
      Some(t) => t,
      _ => {
        early_return!();
      }
    };

    let region = match self.regions.get(&self.place_domain.index(&ptr)) {
      Some(region) => *region,
      None => {
        early_return!();
      }
    };

    let loans = &self.loans[region];
    loans
      .iter()
      .map(|place| {
        let mut projection = place.projection.to_vec();
        projection.extend(projection_past_deref);
        Place {
          local: place.local,
          projection: self.tcx.intern_place_elems(&projection),
        }
      })
      .collect_indices(self.place_domain.clone())
  }

  pub fn conflicts(&self, place: impl ToIndex<Place<'tcx>>) -> Conflicts<'tcx> {
    let place_index = place.to_index(&self.place_domain);
    let compute_conflicts = move || {
      let aliases = self.aliases(place_index);
      debug!(
        "aliases for {:?} are: {:?}",
        self.place_domain.value(place_index),
        aliases
      );

      // TODO: is this correct?
      let single_pointee = {
        // If there is only one pointer at every level of indirection, then
        // there is only one possible place pointed-to
        let deref_counts = aliases
          .iter()
          .map(|place| {
            place
              .projection
              .iter()
              .filter(|elem| *elem == ProjectionElem::Deref)
              .count()
          })
          .collect::<HashSet<_>>();
        deref_counts.len() == aliases.len()
      };

      let (subs, supers): (Vec<_>, Vec<_>) = aliases
        .iter()
        .map(|alias| {
          self
            .place_domain
            .iter_enumerated()
            .filter_map(move |(idx, place)| {
              let relation = PlaceRelation::of(*place, *alias);
              relation.overlaps().then(move || (relation, idx))
            })
        })
        .flatten()
        .partition(|(relation, _)| match relation {
          PlaceRelation::Sub => true,
          PlaceRelation::Super => false,
          PlaceRelation::Disjoint => unreachable!(),
        });

      let to_set = |v: Vec<(PlaceRelation, PlaceIndex)>| {
        v.into_iter()
          .map(|(_, idx)| idx)
          .collect_indices(self.place_domain.clone())
      };

      Conflicts {
        subs: to_set(subs),
        supers: to_set(supers),
        single_pointee,
      }
    };

    self
      .loan_cache
      .borrow_mut()
      .entry(place_index)
      .or_insert_with(compute_conflicts)
      .clone()
  }

  pub fn build(
    tcx: TyCtxt<'tcx>,
    body: &'a Body<'tcx>,
    outlives_constraints: Vec<OutlivesConstraint>,
  ) -> Self {
    let local_projected_regions = body
      .local_decls()
      .indices()
      .map(|local| {
        let place = utils::local_to_place(local, tcx);
        utils::interior_pointers(place, tcx, body)
      })
      .fold(HashMap::default(), |mut h1, h2| {
        h1.extend(h2);
        h1
      });

    let start = Instant::now();
    let max_region = local_projected_regions
      .keys()
      .chain(
        outlives_constraints
          .iter()
          .map(|(r1, r2, _)| vec![r1, r2].into_iter())
          .flatten(),
      )
      .map(|region| region.as_usize())
      .max()
      .unwrap_or(0)
      + 1;

    let root_region = RegionVid::from_usize(0);
    let processed_constraints = outlives_constraints
      .iter()
      // TODO: EXPLAIN THE ORDERING?
      .map(|(r1, r2, _)| (*r1, *r2))
      // static region outlives everything
      .chain((1..max_region).map(|i| (root_region, RegionVid::from_usize(i))))
      .collect();
    let region_graph = VecGraph::new(max_region, processed_constraints);
    let constraint_sccs: Sccs<_, ConstraintSccIndex> = Sccs::new(&region_graph);

    let mut regions_in_scc =
      IndexVec::from_elem_n(BitSet::new_empty(max_region), constraint_sccs.num_sccs());
    {
      let regions_in_constraint = outlives_constraints
        .iter()
        .map(|constraint| vec![constraint.0, constraint.1].into_iter())
        .flatten()
        .collect::<HashSet<_>>();
      for region in 0..max_region {
        let region = RegionVid::from_usize(region);
        if regions_in_constraint.contains(&region) {
          let scc = constraint_sccs.scc(region);
          regions_in_scc[scc].insert(region);
        }
      }
    }
    debug!("regions_in_scc: {:?}", regions_in_scc);

    let root_scc = constraint_sccs.scc(root_region);
    let region_ancestors =
      Self::compute_region_ancestors(&constraint_sccs, &regions_in_scc, root_scc);
    debug!("region ancestors: {:?}", region_ancestors);

    let mut place_collector = utils::PlaceCollector::default();
    place_collector.visit_body(body);

    let place_domain = {
      let all_places = body
        .local_decls()
        .indices()
        .map(|local| {
          utils::interior_places(utils::local_to_place(local, tcx), tcx, body).into_iter()
        })
        .flatten()
        .collect::<Vec<_>>();

      debug!("Place domain size: {}", all_places.len());

      Rc::new(PlaceDomain::new(tcx, all_places))
    };

    let mut aliases = Aliases {
      loans: IndexVec::from_elem_n(PlaceSet::new(place_domain.clone()), max_region),
      loan_cache: RefCell::new(IndexMap::default()),
      loan_locals: SparseBitMatrix::new(max_region),
      place_domain: place_domain.clone(),
      regions: local_projected_regions
        .iter()
        .map(|(region, (place, _))| (place_domain.index(place), *region))
        .collect(),
      tcx,
    };

    for (region, (sub_place, _)) in local_projected_regions {
      aliases.loans[region].insert(place_domain.index(&tcx.mk_place_deref(sub_place)));
    }

    let mut gather_borrows = GatherBorrows {
      borrows: Vec::new(),
    };
    gather_borrows.visit_body(body);
    for (region, _, place) in gather_borrows.borrows.into_iter() {
      aliases.loans[region].insert(place_domain.index(&place));
    }

    elapsed("Alias setup", start);

    debug!("initial aliases {:?}", aliases.loans);

    let start = Instant::now();
    loop {
      let mut changed = false;
      let prev_aliases = aliases.loans.clone();
      for (region, places) in aliases.loans.iter_enumerated_mut() {
        let alias_places = region_ancestors
          .get(&region)
          .map(|sccs| {
            let alias_regions = sccs
              .iter()
              .map(|scc_index| regions_in_scc[scc_index].iter())
              .flatten();

            alias_regions
              .filter_map(|region| prev_aliases.get(region).map(|places| places.indices()))
              .flatten()
              .collect_indices(place_domain.clone())
          })
          .unwrap_or_else(|| PlaceSet::new(place_domain.clone()));

        changed = places.union(&alias_places);
      }

      if !changed {
        break;
      }
    }

    for (region, loans) in aliases.loans.iter_enumerated() {
      for loan in loans.iter() {
        aliases.loan_locals.insert(loan.local, region);
      }
    }

    elapsed("Alias compute", start);

    debug!(
      "Aliases: {}",
      aliases
        .loans
        .iter_enumerated()
        .map(|(region, places)| format!("{:?}: {:?}", region, places))
        .collect::<Vec<_>>()
        .join(", ")
    );

    aliases
  }
}
