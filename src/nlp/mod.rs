//! Nonlinear programming: expression graphs with derivatives (`graph`).

#[allow(dead_code)] // the evaluator's users (a nonlinear solver) come next
pub mod graph;

#[cfg(test)]
mod tests {
    use super::graph::Evaluator;
    use crate::model;
    use crate::problem::Problem;
    use serde_json::Value;

    /// Expressions in the problem format's grammar, built through the model
    /// (parser, affine/nonlinear split, graph), have the value, gradient, and
    /// Hessian that SymPy computed (tests/data/sympy_derivatives.json, from
    /// tools/sympy_derivatives.py).
    #[test]
    fn derivatives_match_sympy() {
        let data: Value = serde_json::from_str(include_str!("../../tests/data/sympy_derivatives.json")).unwrap();
        let cases = data["cases"].as_array().unwrap();
        assert!(cases.len() >= 50);
        let close = |a: f64, b: f64| (a - b).abs() <= 1e-9 * (1.0 + a.abs().max(b.abs()));
        let numbers = |v: &Value| -> Vec<f64> { v.as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).collect() };
        for (k, case) in cases.iter().enumerate() {
            let text = case["expr"].as_str().unwrap();
            let json = format!(
                r#"{{"version": "1.2", "variables": [{{"name": "x1", "type": "continuous"}},
                    {{"name": "x2", "type": "continuous"}}, {{"name": "x3", "type": "continuous"}}],
                    "constraints": [], "solve": {{"method": "minimize", "objective": {}}}}}"#,
                serde_json::to_string(text).unwrap()
            );
            let m = model::build(&Problem::from_json(&json).unwrap()).unwrap_or_else(|e| panic!("case {k}: {e}"));
            let x = numbers(&case["point"]);
            let what = format!("case {k}: {text}");
            assert!(close(m.objective_value(&x), case["value"].as_f64().unwrap()), "{what}: value");
            let mut grad = m.obj.clone();
            let mut hess = vec![vec![0.0; 3]; 3];
            if let Some(nl) = &m.nonlinear {
                let mut ev = Evaluator::new(&nl.graph, &[nl.objective.unwrap()], 3);
                ev.eval(&x);
                for (j, v) in ev.gradient(0) {
                    grad[j] += v;
                }
                let values = ev.hessian(&[1.0]);
                for (&(i, j), v) in ev.hessian_pattern().iter().zip(values) {
                    hess[i][j] = v;
                    hess[j][i] = v;
                }
            }
            for (j, want) in numbers(&case["gradient"]).into_iter().enumerate() {
                assert!(close(grad[j], want), "{what}: d/dx{} = {} vs {want}", j + 1, grad[j]);
            }
            for (i, row) in case["hessian"].as_array().unwrap().iter().enumerate() {
                for (j, want) in numbers(row).into_iter().enumerate() {
                    assert!(close(hess[i][j], want), "{what}: H[{i}][{j}] = {} vs {want}", hess[i][j]);
                }
            }
        }
    }
}
