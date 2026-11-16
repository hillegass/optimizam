//! Writing a model as an MPS file ([`Model::to_mps`]), so that another
//! solver can be given the same model.
//!
//! What is written is the solver's own linear model (`lower.rs`'s
//! `LinearModel`, with the scheduling constraints encoded as the MILP engine
//! gets them), not the builder's: every kind of constraint is there as the
//! rows and columns it comes to, so whatever the format learns to say, this
//! writer says it without being told.

use super::builder::{Model, label_of};
use super::family::Element;
use super::{Error, ErrorKind};
use crate::model::{LinearModel, Sense};
use std::collections::HashSet;
use std::fmt::Write;

/// Words that begin a section in one MPS reader or another. No name of the
/// file is one of them, in case a reader takes a line for a section's start
/// by its first word alone (HiGHS doesn't).
const KEYWORDS: &[&str] = &[
    "NAME",
    "ROWS",
    "COLUMNS",
    "RHS",
    "RANGES",
    "BOUNDS",
    "ENDATA",
    "OBJSENSE",
    "OBJSENSEMAX",
    "OBJSENSEMIN",
    "MAX",
    "MIN",
    "MAXIMIZE",
    "MINIMIZE",
    "MARKER",
    "SOS",
    "INDICATORS",
    "LAZYCONS",
    "USERCUTS",
    "GENCONS",
    "QUADOBJ",
    "QMATRIX",
    "QSECTION",
    "QCMATRIX",
    "CSECTION",
    "PWLOBJ",
];

/// Names for one of the file's kinds of things (rows, or columns), each a
/// single word that no other has.
#[derive(Default)]
struct Names {
    used: HashSet<String>,
}

impl Names {
    /// `wanted` as a name of the file: without white space (fields are
    /// separated by it), not a section's keyword, and not yet taken.
    fn take(&mut self, wanted: &str) -> String {
        let mut name: String =
            wanted.chars().map(|c| if c.is_whitespace() || c.is_control() { '_' } else { c }).collect();
        if name.is_empty() || KEYWORDS.contains(&name.to_ascii_uppercase().as_str()) {
            name.push('_');
        }
        if self.used.contains(&name) {
            let base = name.clone();
            let mut k = 2;
            while self.used.contains(&name) {
                name = format!("{base}_{k}");
                k += 1;
            }
        }
        self.used.insert(name.clone());
        name
    }
}

/// A number as the file has it: the shortest text that reads back as the
/// same number, with an exponent where the digits would run long.
fn number(x: f64) -> String {
    if x == 0.0 {
        "0".into()
    } else if x.abs() >= 1e16 || x.abs() < 1e-5 {
        format!("{x:e}")
    } else {
        format!("{x}")
    }
}

/// A row of the file: its name, its type, and its right-hand side.
struct RowOut {
    name: String,
    /// `L`, `G`, or `E`.
    kind: char,
    rhs: f64,
    /// For a row with two different finite limits (written as an `L` row):
    /// how far below the right-hand side the lower one is.
    range: Option<f64>,
}

impl RowOut {
    /// The row `lower <= .. <= upper`, or None for a row with no limit.
    fn new(name: String, lower: f64, upper: f64) -> Option<RowOut> {
        let (kind, rhs, range) = match (lower.is_finite(), upper.is_finite()) {
            (false, false) => return None,
            (true, true) if lower == upper => ('E', lower, None),
            (true, true) => ('L', upper, Some(upper - lower)),
            (false, true) => ('L', upper, None),
            (true, false) => ('G', lower, None),
        };
        Some(RowOut { name, kind, rhs, range })
    }
}

/// A column of the file.
struct ColOut {
    name: String,
    integer: bool,
    lower: f64,
    upper: f64,
    cost: f64,
    /// Its coefficients, by the row's place in the file.
    entries: Vec<(usize, f64)>,
}

impl Model {
    /// The model as an MPS file (free format), for another solver to read:
    /// to check an answer, or to compare how a search goes.
    ///
    /// The file holds the linear model that optimizam's own MILP engine
    /// would be given. Variables are columns called as results call them
    /// (`make[north,3]`), and each comparison is a row under its
    /// constraint's name (`capacity[north]`, or `constraints[3]` by its
    /// place for one without a name). What MPS has no words for is written
    /// as the rows and columns it is encoded with, whose names begin with
    /// `_`: `all_different`, `table`, `element`, `no_overlap`, `cumulative`,
    /// `enforce_if`, `!=`, and `abs`, `min`, and `max` of variables (a
    /// symbolic variable is an integer column, 1 for its set's first
    /// element), a domain with gaps, and the rows that break value
    /// symmetry. A maximization is written with an `OBJSENSE` section, and a
    /// constant in the objective as minus the objective row's right-hand
    /// side.
    ///
    /// A name is one word in such a file, so white space in a set's element
    /// becomes `_`, and a name that two things would share gets a number
    /// after it. The file doesn't say how many solutions are asked for: a
    /// reader will look for one.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Unsupported`] for a model with nonlinear terms, and
    /// whatever error solving the model would give for a model that can't
    /// be solved as it is.
    ///
    /// ```
    /// use optimizam::{Model, constraint, var};
    ///
    /// let mut m = Model::new();
    /// let x = m.add_var(var().integer().bounds(0, 4).name("x"));
    /// let y = m.add_var(var().bounds(0, 2.5).name("y"));
    /// m.add(constraint!(x + 2 * y <= 6).name("cap"));
    /// m.maximize(3 * x + y);
    /// let mps = m.to_mps()?;
    /// assert!(mps.contains(" L  cap\n"));
    /// assert!(mps.contains("    x cap 1\n"));
    /// assert!(mps.contains(" UP BND y 2.5\n"));
    /// # Ok::<(), optimizam::Error>(())
    /// ```
    pub fn to_mps(&self) -> Result<String, Error> {
        let mut lm = super::lower::lower(self).map_err(Error::from_solver)?;
        if let Some(nonlinear) = &lm.nonlinear {
            return Err(Error::new(
                ErrorKind::Unsupported,
                format!("unsupported: an MPS file holds a linear model, and {} is nonlinear", nonlinear.first_place()),
            ));
        }
        lm.encode_scheduling().map_err(Error::from_solver)?;
        Ok(write_mps(self, &lm))
    }
}

/// The columns' names: the model's variables as results write them, and
/// `_c7` (by the column's place) for a column an encoding added.
fn column_names(lm: &LinearModel) -> Vec<String> {
    let mut wanted: Vec<Option<String>> = vec![None; lm.num_cols()];
    for var in &lm.vars {
        for (key, col) in &var.elements {
            let key: Vec<Element> = key.iter().map(Element::from_elem).collect();
            wanted[*col] = Some(label_of(&var.name, &key));
        }
    }
    let mut names = Names::default();
    wanted.into_iter().enumerate().map(|(j, name)| names.take(&name.unwrap_or_else(|| format!("_c{j}")))).collect()
}

fn write_mps(model: &Model, lm: &LinearModel) -> String {
    let mut row_names = Names::default();
    let objective = row_names.take("objective");
    let mut rows: Vec<RowOut> = Vec::with_capacity(lm.rows.len());
    let mut cols: Vec<ColOut> = column_names(lm)
        .into_iter()
        .enumerate()
        .map(|(j, name)| ColOut {
            name,
            integer: lm.col_integer[j],
            lower: lm.col_lower[j],
            upper: lm.col_upper[j],
            cost: lm.obj.get(j).copied().unwrap_or(0.0),
            entries: Vec::new(),
        })
        .collect();
    for (i, row) in lm.rows.iter().enumerate() {
        // The first rows are the model's comparisons; the rest are encodings.
        let wanted = match lm.row_items.get(i) {
            Some(&item) => model.constraint_label(item),
            None => format!("_r{i}"),
        };
        // A row without a limit says nothing, and a second row of type N
        // would be read as another objective.
        let Some(out) = RowOut::new(row_names.take(&wanted), row.lower, row.upper) else { continue };
        for &(j, v) in row.coefs.iter().filter(|c| c.1 != 0.0) {
            // A column named twice in a row comes twice in a row here.
            match cols[j].entries.last_mut() {
                Some(last) if last.0 == rows.len() => last.1 += v,
                _ => cols[j].entries.push((rows.len(), v)),
            }
        }
        rows.push(out);
    }
    // A domain with gaps: one binary column for each of its ranges, one of
    // which is 1, and the variable between that range's ends.
    let mut col_names = Names { used: cols.iter().map(|c| c.name.clone()).collect() };
    for j in 0..lm.num_cols() {
        let Some(domain) = &lm.col_domain[j] else { continue };
        let ranges: Vec<(f64, f64)> = domain
            .iter()
            .map(|&(a, b)| ((a as f64).max(cols[j].lower), (b as f64).min(cols[j].upper)))
            .filter(|(a, b)| a <= b)
            .collect();
        if let [(a, b)] = ranges[..] {
            (cols[j].lower, cols[j].upper) = (a, b);
            continue;
        }
        let name = cols[j].name.clone();
        let (one, low, high) = (rows.len(), rows.len() + 1, rows.len() + 2);
        rows.push(RowOut { name: row_names.take(&format!("_one_{name}")), kind: 'E', rhs: 1.0, range: None });
        rows.push(RowOut { name: row_names.take(&format!("_low_{name}")), kind: 'G', rhs: 0.0, range: None });
        rows.push(RowOut { name: row_names.take(&format!("_high_{name}")), kind: 'L', rhs: 0.0, range: None });
        cols[j].entries.extend([(low, 1.0), (high, 1.0)]);
        for (k, &(a, b)) in ranges.iter().enumerate() {
            let entries = [(one, 1.0), (low, -a), (high, -b)].into_iter().filter(|e| e.1 != 0.0).collect();
            cols.push(ColOut {
                name: col_names.take(&format!("_in_{name}_{}", k + 1)),
                integer: true,
                lower: 0.0,
                upper: 1.0,
                cost: 0.0,
                entries,
            });
        }
    }
    // A constraint without variables that doesn't hold (`0 >= 1`) is no row
    // of the solver's model: here it is one that nothing can satisfy.
    if lm.violated_constant_row.is_some() {
        rows.push(RowOut { name: row_names.take("_violated"), kind: 'E', rhs: 1.0, range: None });
    }

    let mut out = String::new();
    writeln!(out, "* Written by optimizam {}.", env!("CARGO_PKG_VERSION")).unwrap();
    if let Some(what) = &lm.violated_constant_row {
        let what: String = what.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
        writeln!(out, "* Infeasible as written ({what}): the row _violated says 0 = 1.").unwrap();
    }
    let name = Names::default().take(model.name().unwrap_or("optimizam"));
    writeln!(out, "NAME {name}").unwrap();
    if lm.sense == Sense::Maximize {
        out.push_str("OBJSENSE\n    MAX\n");
    }
    out.push_str("ROWS\n");
    writeln!(out, " N  {objective}").unwrap();
    for row in &rows {
        writeln!(out, " {}  {}", row.kind, row.name).unwrap();
    }
    out.push_str("COLUMNS\n");
    let mut in_integers = false;
    let mut markers = 0;
    for col in &cols {
        if col.integer != in_integers {
            in_integers = col.integer;
            let which = if in_integers { "INTORG" } else { "INTEND" };
            writeln!(out, "    MARKER{markers} 'MARKER' '{which}'").unwrap();
            markers += 1;
        }
        if col.cost != 0.0 || col.entries.is_empty() {
            // A column in no row is still a column: it is named here once.
            writeln!(out, "    {} {objective} {}", col.name, number(col.cost)).unwrap();
        }
        for &(i, v) in col.entries.iter().filter(|e| e.1 != 0.0) {
            writeln!(out, "    {} {} {}", col.name, rows[i].name, number(v)).unwrap();
        }
    }
    if in_integers {
        writeln!(out, "    MARKER{markers} 'MARKER' 'INTEND'").unwrap();
    }
    out.push_str("RHS\n");
    if lm.obj_offset != 0.0 {
        // By convention the objective's constant is minus this number.
        writeln!(out, "    RHS {objective} {}", number(-lm.obj_offset)).unwrap();
    }
    for row in rows.iter().filter(|r| r.rhs != 0.0) {
        writeln!(out, "    RHS {} {}", row.name, number(row.rhs)).unwrap();
    }
    if rows.iter().any(|r| r.range.is_some()) {
        out.push_str("RANGES\n");
        for row in &rows {
            if let Some(range) = row.range {
                writeln!(out, "    RNG {} {}", row.name, number(range)).unwrap();
            }
        }
    }
    out.push_str("BOUNDS\n");
    for col in &cols {
        let (name, lower, upper) = (&col.name, col.lower, col.upper);
        if lower == upper {
            writeln!(out, " FX BND {name} {}", number(lower)).unwrap();
        } else if lower == f64::NEG_INFINITY && upper == f64::INFINITY {
            writeln!(out, " FR BND {name}").unwrap();
        } else {
            // The upper bound first, and a lower bound of 0 said outright
            // under a negative one: some readers take a negative upper
            // bound alone to mean no lower bound. An integer column says
            // that it has no upper bound: with no bound at all, some
            // readers take it for a binary.
            if upper.is_finite() {
                writeln!(out, " UP BND {name} {}", number(upper)).unwrap();
            } else if col.integer {
                writeln!(out, " PL BND {name}").unwrap();
            }
            if lower == f64::NEG_INFINITY {
                writeln!(out, " MI BND {name}").unwrap();
            } else if lower != 0.0 || upper < 0.0 {
                writeln!(out, " LO BND {name} {}", number(lower)).unwrap();
            }
        }
    }
    out.push_str("ENDATA\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_single_words_that_differ() {
        let mut names = Names::default();
        assert_eq!(names.take("ship[New York,3]"), "ship[New_York,3]");
        assert_eq!(names.take("ship[New_York,3]"), "ship[New_York,3]_2");
        assert_eq!(names.take("ship[New York,3]"), "ship[New_York,3]_3");
        assert_eq!(names.take("rhs"), "rhs_");
        assert_eq!(names.take(""), "_");
    }

    #[test]
    fn numbers_read_back() {
        for x in [1.0, -2.5, 0.1, 1e-7, 123456789.125, 1e16, -3e300, 1.0 / 3.0] {
            assert_eq!(number(x).parse::<f64>().unwrap(), x, "{}", number(x));
        }
        assert_eq!(
            (number(-0.0), number(12.0), number(1e20), number(2.5e-9)),
            ("0".into(), "12".into(), "1e20".into(), "2.5e-9".into())
        );
    }

    #[test]
    fn rows_have_the_type_their_limits_give() {
        let row = |lower, upper| RowOut::new("r".into(), lower, upper).map(|r| (r.kind, r.rhs, r.range));
        let inf = f64::INFINITY;
        assert_eq!(row(-inf, 4.0), Some(('L', 4.0, None)));
        assert_eq!(row(1.0, inf), Some(('G', 1.0, None)));
        assert_eq!(row(2.0, 2.0), Some(('E', 2.0, None)));
        assert_eq!(row(-1.0, 4.0), Some(('L', 4.0, Some(5.0))));
        assert_eq!(row(-inf, inf), None);
    }
}
