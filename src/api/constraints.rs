//! What a solution leaves of each constraint: its slack, and for a linear
//! program the constraints' duals. Worked out when [`Options::constraints`]
//! asks, on the builder [`Model`], whose constraints the result then names.
//!
//! [`Options::constraints`]: super::Options::constraints

use super::builder::{Item, Model};
use super::expression::{Constraint, Literal, LiteralKind};
use super::family::Element;
use crate::model::LinearModel;
use crate::solve::Duals;
use std::collections::HashMap;

/// The model's comparison (`expr`) constraints, as a result reports them.
#[derive(Debug)]
pub(crate) struct Table {
    /// Their places among the model's items (the numbers of their ids), in order.
    pub ids: Vec<usize>,
    /// Their names (`capacity[north]`, or `constraints[3]` without one).
    pub labels: Vec<String>,
    /// Their duals, for a linear program: None for one that is no row of it
    /// (a comparison of constants).
    pub duals: Option<Vec<Option<f64>>>,
}

impl Table {
    /// The table of `model`, with the duals of its rows in `lowered`, if any.
    pub fn new(model: &Model, lowered: &LinearModel, duals: Option<&Duals>) -> Table {
        let ids: Vec<usize> = (0..model.items.len()).filter(|&i| matches!(model.items[i], Item::Expr(_))).collect();
        let labels = ids.iter().map(|&i| model.constraint_label(i)).collect();
        let duals = duals.map(|duals| {
            let of_item: HashMap<usize, f64> =
                lowered.row_items.iter().copied().zip(duals.rows.iter().copied()).collect();
            ids.iter().map(|i| of_item.get(i).copied()).collect()
        });
        Table { ids, labels, duals }
    }

    /// Where constraint `id` is in the table.
    pub fn position(&self, id: usize) -> Option<usize> {
        self.ids.binary_search(&id).ok()
    }

    /// Each constraint's slack at a solution's `values`: how far its
    /// expression is from the nearer of its bounds (0 when it is tight).
    /// None for one that isn't enforced there (an `only_if` literal is false).
    pub fn slacks(&self, model: &Model, values: &[f64]) -> Vec<Option<f64>> {
        let slack = |c: &Constraint| {
            if !c.literals.iter().all(|literal| holds(model, literal, values)) {
                return None;
            }
            if c.element.is_some() {
                return Some(0.0); // `v.equals(e)` holds exactly
            }
            let at = c.expr.evaluate(&|col| values[col]);
            Some((at - c.lower).min(c.upper - at))
        };
        let expr = |&i: &usize| match &model.items[i] {
            Item::Expr(c) => slack(c),
            _ => None,
        };
        self.ids.iter().map(expr).collect()
    }
}

/// Whether a literal holds at `values`.
fn holds(model: &Model, literal: &Literal, values: &[f64]) -> bool {
    let col = literal.var.col;
    let value = values[col].round();
    match &literal.kind {
        LiteralKind::True => value == 1.0,
        LiteralKind::False => value == 0.0,
        LiteralKind::Is(e) | LiteralKind::IsNot(e) => {
            // A symbolic variable's value is its element's place in the set.
            let (family, k) = model.family_of(col);
            let target = match (&family.spec(k).values, e) {
                (Some(set), e) => set.position(e).map(|p| p as f64 + 1.0),
                (None, Element::Int(i)) => Some(*i as f64),
                (None, Element::Str(_)) => None,
            };
            (target == Some(value)) == matches!(literal.kind, LiteralKind::Is(_))
        }
    }
}
