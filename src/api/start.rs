//! Starting values after the model is made: [`Model::set_start`], and the
//! values of a solution or of a result file as the starts of the variables
//! they name ([`Model::start_from`], [`Model::start_from_result`]).

use super::builder::Model;
use super::expression::{Number, Variable};
use super::family::Element;
use super::solution::Solution;
use super::{Error, ErrorKind};
use serde_json::Value;

impl Model {
    /// Gives a variable a starting value, in place of the one its spec has
    /// ([`VarSpec::start`](crate::VarSpec::start)), which says what a
    /// start is for. For a symbolic variable the value is its element's
    /// place in the set, from 1; [`Model::start_from`] takes elements.
    ///
    /// # Panics
    ///
    /// If `v` belongs to another model, or `value` isn't a finite number.
    pub fn set_start(&mut self, v: Variable, value: impl Number) {
        assert!(v.model == self.id && v.col < self.num_cols, "the variable belongs to another model");
        let value = value.to_f64();
        assert!(value.is_finite(), "a start is a finite number, not {value}");
        self.starts.insert(v.col, value);
    }

    /// Starts every variable at the value `solution` has for it, and
    /// returns how many variables got one. The solution may be of this
    /// model as it was before an edit, or of another: a value is taken
    /// where the name and the key are the same (and, for a symbolic
    /// variable, where its element is one of this variable's set).
    ///
    /// This is how a solve goes on from the last one: after a constraint
    /// was added or a bound moved, the old solution is where to look first.
    ///
    /// ```
    /// use optimizam::{Model, Options, constraint, var};
    ///
    /// let mut m = Model::new();
    /// let x = m.add_vars(3, var().integer().bounds(0, 5).name("x"));
    /// m.add(constraint!(x[0] + x[1] + x[2] <= 9));
    /// m.maximize(3 * x[0] + 2 * x[1] + x[2]);
    /// let first = m.solve(&Options::default())?;
    ///
    /// m.add(constraint!(x[0] - x[1] <= 1));
    /// assert_eq!(m.start_from(first.best().unwrap()), 3);
    /// let second = m.solve(&Options::default())?;
    /// assert_eq!(second.objective(), Some(23.0));
    /// # Ok::<(), optimizam::Error>(())
    /// ```
    pub fn start_from(&mut self, solution: &Solution) -> usize {
        let mut starts = Vec::new();
        for f in &self.families {
            let (name, set) = (f.name(), f.spec(0).values.as_ref());
            for k in 0..f.len {
                if let Some(value) = solution.start_for(&name, &f.key(k), set) {
                    starts.push((f.first + k, value));
                }
            }
        }
        let count = starts.len();
        self.starts.extend(starts);
        count
    }

    /// Starts every variable at the value that a result file's first
    /// solution has for it (`docs/result-format.md`; the text of the file),
    /// and returns how many variables got one: as [`Model::start_from`],
    /// for a solution that was written out, by this program or another. A
    /// result written without its zeros (`"omitted": ["zeros"]`) gives 0 to
    /// the elements it leaves out.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Parse`] if the text isn't JSON, and
    /// [`ErrorKind::Invalid`] if it has no solution.
    pub fn start_from_result(&mut self, result: &str) -> Result<usize, Error> {
        let result: Value = serde_json::from_str(result)
            .map_err(|e| Error::new(ErrorKind::Parse, format!("invalid result file: {e}")))?;
        let variables = result
            .get("solutions")
            .and_then(|s| s.get(0))
            .and_then(|s| s.get("variables"))
            .and_then(Value::as_object)
            .ok_or_else(|| Error::new(ErrorKind::Invalid, "the result file has no solution to start from"))?;
        let zeros = result.get("omitted").and_then(Value::as_array).is_some_and(|o| o.iter().any(|x| x == "zeros"));
        let mut starts = Vec::new();
        for f in &self.families {
            let (top, set) = (variables.get(&f.name()), f.spec(0).values.as_ref());
            for k in 0..f.len {
                let entry = f.key(k).iter().fold(top, |node, e| node?.get(e.to_string()));
                let value = match (entry, set) {
                    (Some(Value::Number(n)), None) => n.as_f64(),
                    // An entry that was left out for being zero.
                    (None, None) if zeros && f.indexed() => Some(0.0),
                    (Some(Value::String(s)), Some(set)) => set.position(s.as_str()).map(|p| (p + 1) as f64),
                    (Some(Value::Number(n)), Some(set)) => {
                        n.as_i64().and_then(|i| set.position(Element::Int(i))).map(|p| (p + 1) as f64)
                    }
                    _ => None,
                };
                if let Some(value) = value.filter(|v| v.is_finite()) {
                    starts.push((f.first + k, value));
                }
            }
        }
        let count = starts.len();
        self.starts.extend(starts);
        Ok(count)
    }
}
