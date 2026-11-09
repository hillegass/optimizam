//! Index sets and families of variables indexed by them.

use super::expression::Variable;
use std::collections::HashMap;
use std::fmt;
use std::ops::Index;
use std::sync::Arc;

/// An element of a [`Set`]: an integer or a string, as in problem files.
///
/// Converts from integers and strings, so keys can be written as they are:
/// `x[("north", 3)]`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Element {
    /// An integer element.
    Int(i64),
    /// A string element.
    Str(Arc<str>),
}

impl fmt::Display for Element {
    /// The element as results key it: `3` or `north`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Element::Int(i) => write!(f, "{i}"),
            Element::Str(s) => f.write_str(s),
        }
    }
}

macro_rules! element_from_int {
    ($($t:ty),*) => {$(
        impl From<$t> for Element {
            fn from(i: $t) -> Element {
                Element::Int(i as i64)
            }
        }
    )*};
}
element_from_int!(i8, i16, i32, i64, u8, u16, u32);

impl From<usize> for Element {
    /// # Panics
    ///
    /// If `i` doesn't fit in an `i64`.
    fn from(i: usize) -> Element {
        Element::Int(i64::try_from(i).expect("an element too large for i64"))
    }
}

impl From<&str> for Element {
    fn from(s: &str) -> Element {
        Element::Str(s.into())
    }
}

impl From<String> for Element {
    fn from(s: String) -> Element {
        Element::Str(s.into())
    }
}

impl From<&String> for Element {
    fn from(s: &String) -> Element {
        Element::Str(s.as_str().into())
    }
}

impl From<&Element> for Element {
    fn from(e: &Element) -> Element {
        e.clone()
    }
}

impl Element {
    pub(crate) fn to_elem(&self) -> crate::problem::Elem {
        match self {
            Element::Int(i) => crate::problem::Elem::Int(*i),
            Element::Str(s) => crate::problem::Elem::Str(s.to_string()),
        }
    }

    pub(crate) fn from_elem(e: &crate::problem::Elem) -> Element {
        match e {
            crate::problem::Elem::Int(i) => Element::Int(*i),
            crate::problem::Elem::Str(s) => Element::Str(s.as_str().into()),
        }
    }
}

/// A named, ordered set of distinct elements, to index variable families
/// with ([`Model::add_family`](crate::Model::add_family)) or to give a
/// symbolic variable its values ([`VarSpec::values`](crate::VarSpec::values)).
///
/// Cheap to clone (the elements are shared). The name is what an exported
/// problem file calls the set.
///
/// ```
/// use optimizam::Set;
///
/// let plants = Set::new("PLANTS", ["north", "south"]);
/// let hours = Set::range("HOURS", 1..=24);
/// assert_eq!((plants.len(), hours.len()), (2, 24));
/// assert_eq!(plants.position("south"), Some(1));
/// ```
#[derive(Clone)]
pub struct Set(Arc<SetData>);

struct SetData {
    name: String,
    elements: Vec<Element>,
    ints: HashMap<i64, usize>,
    strs: HashMap<Arc<str>, usize>,
}

impl Set {
    /// A set of these elements, in this order.
    ///
    /// # Panics
    ///
    /// If an element repeats, or two elements would have the same key in
    /// results (the integer `3` and the string `"3"`).
    pub fn new(name: impl Into<String>, elements: impl IntoIterator<Item: Into<Element>>) -> Set {
        let name = name.into();
        let elements: Vec<Element> = elements.into_iter().map(Into::into).collect();
        let (mut ints, mut strs) = (HashMap::new(), HashMap::new());
        let mut keys = std::collections::HashSet::new();
        for (k, e) in elements.iter().enumerate() {
            assert!(
                keys.insert(e.to_string()),
                "set {name}: the element {e} appears twice (or as a number and a string)"
            );
            match e {
                Element::Int(i) => ints.insert(*i, k),
                Element::Str(s) => strs.insert(s.clone(), k),
            };
        }
        Set(Arc::new(SetData { name, elements, ints, strs }))
    }

    /// The integers in `range`, such as `1..=24`.
    pub fn range(name: impl Into<String>, range: impl IntoIterator<Item = i64>) -> Set {
        Set::new(name, range)
    }

    /// The set's name.
    pub fn name(&self) -> &str {
        &self.0.name
    }

    /// The elements, in order.
    pub fn elements(&self) -> &[Element] {
        &self.0.elements
    }

    /// The number of elements.
    pub fn len(&self) -> usize {
        self.0.elements.len()
    }

    /// Whether the set has no elements.
    pub fn is_empty(&self) -> bool {
        self.0.elements.is_empty()
    }

    /// The position of an element (from 0), or None if it isn't in the set.
    pub fn position(&self, element: impl KeyPart) -> Option<usize> {
        element.position_in(self)
    }

    /// Whether some element is a string (so variables over the set are
    /// symbolic).
    pub(crate) fn has_strings(&self) -> bool {
        !self.0.strs.is_empty()
    }

    pub(crate) fn same(&self, other: &Set) -> bool {
        Arc::ptr_eq(&self.0, &other.0) || (self.0.name == other.0.name && self.0.elements == other.0.elements)
    }
}

impl fmt::Debug for Set {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Set({}: ", self.0.name)?;
        f.debug_list().entries(self.0.elements.iter().map(|e| e.to_string())).finish()?;
        f.write_str(")")
    }
}

mod sealed {
    pub trait Sealed {}
}

/// One element of a key: an integer, a string, or an [`Element`].
pub trait KeyPart: sealed::Sealed {
    /// The element's position in `set`.
    #[doc(hidden)]
    fn position_in(&self, set: &Set) -> Option<usize>;
}

macro_rules! int_key_part {
    ($($t:ty),*) => {$(
        impl sealed::Sealed for $t {}
        impl KeyPart for $t {
            fn position_in(&self, set: &Set) -> Option<usize> {
                i64::try_from(*self).ok().and_then(|i| set.0.ints.get(&i).copied())
            }
        }
    )*};
}
int_key_part!(i8, i16, i32, i64, u8, u16, u32, u64, usize);

impl sealed::Sealed for &str {}
impl KeyPart for &str {
    fn position_in(&self, set: &Set) -> Option<usize> {
        set.0.strs.get(*self).copied()
    }
}

impl sealed::Sealed for String {}
impl KeyPart for String {
    fn position_in(&self, set: &Set) -> Option<usize> {
        set.0.strs.get(self.as_str()).copied()
    }
}

impl sealed::Sealed for &String {}
impl KeyPart for &String {
    fn position_in(&self, set: &Set) -> Option<usize> {
        set.0.strs.get(self.as_str()).copied()
    }
}

impl sealed::Sealed for Element {}
impl KeyPart for Element {
    fn position_in(&self, set: &Set) -> Option<usize> {
        match self {
            Element::Int(i) => set.0.ints.get(i).copied(),
            Element::Str(s) => set.0.strs.get(s).copied(),
        }
    }
}

impl sealed::Sealed for &Element {}
impl KeyPart for &Element {
    fn position_in(&self, set: &Set) -> Option<usize> {
        (*self).position_in(set)
    }
}

/// A key into a [`VarFamily`]: one element per index set. A single element
/// for one set (`x["north"]`, `x[3]`), a tuple for up to four
/// (`x[("north", 3)]`), `()` for a family without index sets, or a slice or
/// array of [`Element`]s (or of one kind of [`KeyPart`]) for any number.
pub trait Key: sealed::Sealed {
    /// The key's position in the product of `sets` (row-major), or None if
    /// it has the wrong number of elements or one isn't in its set.
    #[doc(hidden)]
    fn position(&self, sets: &[Set]) -> Option<usize>;
}

impl<T: KeyPart> Key for T {
    fn position(&self, sets: &[Set]) -> Option<usize> {
        match sets {
            [set] => self.position_in(set),
            _ => None,
        }
    }
}

/// Row-major position of `parts` (each already a position in its set).
fn combine(sets: &[Set], parts: impl IntoIterator<Item = Option<usize>>) -> Option<usize> {
    let mut position = 0;
    let mut count = 0;
    for (set, part) in sets.iter().zip(parts) {
        position = position * set.len() + part?;
        count += 1;
    }
    (count == sets.len()).then_some(position)
}

impl sealed::Sealed for () {}
impl Key for () {
    fn position(&self, sets: &[Set]) -> Option<usize> {
        sets.is_empty().then_some(0)
    }
}

macro_rules! tuple_key {
    ($n:literal: $($T:ident $i:tt),*) => {
        impl<$($T: KeyPart),*> sealed::Sealed for ($($T,)*) {}
        impl<$($T: KeyPart),*> Key for ($($T,)*) {
            fn position(&self, sets: &[Set]) -> Option<usize> {
                if sets.len() != $n {
                    return None;
                }
                combine(sets, [$(self.$i.position_in(&sets[$i])),*])
            }
        }
    };
}
tuple_key!(1: A 0);
tuple_key!(2: A 0, B 1);
tuple_key!(3: A 0, B 1, C 2);
tuple_key!(4: A 0, B 1, C 2, D 3);

impl<E: KeyPart> sealed::Sealed for &[E] {}
impl<E: KeyPart> Key for &[E] {
    fn position(&self, sets: &[Set]) -> Option<usize> {
        if self.len() != sets.len() {
            return None;
        }
        combine(sets, self.iter().zip(sets).map(|(e, s)| e.position_in(s)))
    }
}

impl<E: KeyPart, const N: usize> sealed::Sealed for [E; N] {}
impl<E: KeyPart, const N: usize> Key for [E; N] {
    fn position(&self, sets: &[Set]) -> Option<usize> {
        self.as_slice().position(sets)
    }
}

impl<E: KeyPart> sealed::Sealed for Vec<E> {}
impl<E: KeyPart> Key for Vec<E> {
    fn position(&self, sets: &[Set]) -> Option<usize> {
        self.as_slice().position(sets)
    }
}

/// Variables indexed by the product of some sets, made by
/// [`Model::add_family`](crate::Model::add_family): one variable per
/// combination of elements, reported in results as a table keyed by element
/// (`"open": {"north": {"1": 0, ...}, ...}`).
///
/// Index it with a [`Key`] (`x[("north", 3)]`, which panics on an unknown
/// key, like a slice) or look keys up with [`get`](VarFamily::get). Cheap to
/// clone.
///
/// ```
/// use optimizam::{Expression, Model, Options, Set, constraint, var};
///
/// let plants = Set::new("PLANTS", ["north", "south"]);
/// let hours = Set::range("HOURS", 1..=3);
/// let mut m = Model::new();
/// let on = m.add_family("on", &[&plants, &hours], var().binary());
/// for h in 1..=3 {
///     m.add(constraint!(on[("north", h)] + on[("south", h)] >= 1));
/// }
/// m.minimize(on.vars().iter().copied().sum::<Expression>());
/// let result = m.solve(&Options::default())?;
/// assert_eq!(result.objective(), Some(3.0));
/// assert!(on.get(("east", 1)).is_none());
/// # Ok::<(), optimizam::Error>(())
/// ```
#[derive(Clone)]
pub struct VarFamily {
    name: Arc<str>,
    sets: Arc<[Set]>,
    vars: Arc<[Variable]>,
}

impl VarFamily {
    pub(crate) fn new(name: &str, sets: Vec<Set>, vars: Vec<Variable>) -> VarFamily {
        VarFamily { name: name.into(), sets: sets.into(), vars: vars.into() }
    }

    /// The family's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The index sets.
    pub fn sets(&self) -> &[Set] {
        &self.sets
    }

    /// The variable for `key`, or None if the key isn't in the family.
    pub fn get(&self, key: impl Key) -> Option<Variable> {
        key.position(&self.sets).map(|p| self.vars[p])
    }

    /// Every variable, in row-major order of the keys (the last set varies
    /// fastest).
    pub fn vars(&self) -> &[Variable] {
        &self.vars
    }

    /// The number of variables.
    pub fn len(&self) -> usize {
        self.vars.len()
    }

    /// Whether the family has no variables (one of its sets is empty).
    pub fn is_empty(&self) -> bool {
        self.vars.is_empty()
    }

    /// Each key with its variable, in row-major order.
    pub fn iter(&self) -> impl Iterator<Item = (Vec<Element>, Variable)> + '_ {
        self.vars.iter().enumerate().map(|(k, &v)| (key_of(&self.sets, k), v))
    }
}

/// The key at row-major position `k` of the product of `sets`.
pub(crate) fn key_of(sets: &[Set], mut k: usize) -> Vec<Element> {
    let mut key = vec![Element::Int(0); sets.len()];
    for (slot, set) in key.iter_mut().zip(sets).rev() {
        *slot = set.elements()[k % set.len()].clone();
        k /= set.len();
    }
    key
}

impl<K: Key> Index<K> for VarFamily {
    type Output = Variable;

    /// # Panics
    ///
    /// If the key isn't in the family (an element not in its set, or the
    /// wrong number of elements).
    fn index(&self, key: K) -> &Variable {
        match key.position(&self.sets) {
            Some(p) => &self.vars[p],
            None => panic!("no such key in family {} (indexed by {})", self.name, self.set_names()),
        }
    }
}

impl VarFamily {
    fn set_names(&self) -> String {
        if self.sets.is_empty() {
            "nothing: use ()".into()
        } else {
            self.sets.iter().map(|s| s.name()).collect::<Vec<_>>().join(", ")
        }
    }
}

impl fmt::Debug for VarFamily {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VarFamily({}[{}], {} variables)", self.name, self.set_names(), self.vars.len())
    }
}
