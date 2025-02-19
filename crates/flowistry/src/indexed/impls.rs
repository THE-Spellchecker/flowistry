use std::rc::Rc;

use rustc_data_structures::fx::FxHashSet as HashSet;
use rustc_middle::mir::{Body, Local, Location, Place};

use super::{DefaultDomain, IndexSet, IndexedDomain, IndexedValue, OwnedSet, ToIndex};
use crate::{
  mir::utils::{BodyExt, PlaceExt},
  to_index_impl,
};

/// Used to represent dependencies of places.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LocationOrArg {
  Location(Location),
  Arg(Local),
}

impl LocationOrArg {
  pub fn from_place<'tcx>(place: Place<'tcx>, body: &Body<'tcx>) -> Option<Self> {
    place
      .is_arg(body)
      .then_some(LocationOrArg::Arg(place.local))
  }
}

impl From<Location> for LocationOrArg {
  fn from(location: Location) -> Self {
    LocationOrArg::Location(location)
  }
}

impl ToIndex<LocationOrArg> for Location {
  fn to_index(&self, domain: &LocationOrArgDomain) -> LocationOrArgIndex {
    domain.index(&LocationOrArg::Location(*self))
  }
}

impl From<Local> for LocationOrArg {
  fn from(local: Local) -> Self {
    LocationOrArg::Arg(local)
  }
}

impl ToIndex<LocationOrArg> for Local {
  fn to_index(&self, domain: &LocationOrArgDomain) -> LocationOrArgIndex {
    domain.index(&LocationOrArg::Arg(*self))
  }
}

rustc_index::newtype_index! {
  pub struct LocationOrArgIndex {
      DEBUG_FORMAT = "l{}"
  }
}

to_index_impl!(LocationOrArg);

impl IndexedValue for LocationOrArg {
  type Index = LocationOrArgIndex;
  type Domain = LocationOrArgDomain;
}

pub type LocationOrArgSet<S = OwnedSet<LocationOrArg>> = IndexSet<LocationOrArg, S>;
pub type LocationOrArgDomain = DefaultDomain<LocationOrArgIndex, LocationOrArg>;

pub fn build_location_arg_domain(body: &Body) -> Rc<LocationOrArgDomain> {
  let all_locations = body.all_locations().map(LocationOrArg::Location);
  let all_locals = body.args_iter().map(LocationOrArg::Arg);
  let domain = all_locations.chain(all_locals).collect::<Vec<_>>();
  Rc::new(LocationOrArgDomain::new(domain))
}

pub type PlaceSet<'tcx> = HashSet<Place<'tcx>>;
