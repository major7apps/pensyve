use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParameterSweep {
    pub name: String,
    pub values: Vec<f64>,
    pub default_value: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepResult {
    pub parameter: String,
    pub default_value: f64,
    pub optimal_value: f64,
    pub sensitivity_coefficient: f64,
    pub robustness_ratio: f64,
    pub values: Vec<f64>,
    pub metrics: Vec<f64>,
    pub ci_lower: Vec<f64>,
    pub ci_upper: Vec<f64>,
}

/// Sensitivity coefficient via central finite differences at the default value.
pub fn sensitivity_coefficient(param_values: &[f64], metric_values: &[f64], default: f64) -> f64 {
    assert_eq!(param_values.len(), metric_values.len());
    let default_idx = param_values.iter().enumerate()
        .min_by(|(_, a), (_, b)| ((**a) - default).abs().partial_cmp(&((**b) - default).abs()).unwrap())
        .map(|(i, _)| i).unwrap_or(0);

    if default_idx > 0 && default_idx < param_values.len() - 1 {
        let dp = param_values[default_idx + 1] - param_values[default_idx - 1];
        let dm = metric_values[default_idx + 1] - metric_values[default_idx - 1];
        if dp.abs() > 1e-10 { dm / dp } else { 0.0 }
    } else if default_idx == 0 && param_values.len() > 1 {
        let dp = param_values[1] - param_values[0];
        let dm = metric_values[1] - metric_values[0];
        if dp.abs() > 1e-10 { dm / dp } else { 0.0 }
    } else { 0.0 }
}

/// Fraction of sweep range where metric is within `tolerance` of the maximum.
pub fn robustness_ratio(metric_values: &[f64], tolerance: f64) -> f64 {
    if metric_values.is_empty() { return 0.0; }
    let max_val = metric_values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let threshold = max_val * (1.0 - tolerance);
    let within = metric_values.iter().filter(|&&m| m >= threshold).count();
    within as f64 / metric_values.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sensitivity_coefficient() {
        let values = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let metrics = vec![0.5, 1.0, 1.5, 2.0, 2.5];
        let coeff = sensitivity_coefficient(&values, &metrics, 3.0);
        assert!((coeff - 0.5).abs() < 0.2);
    }

    #[test]
    fn test_robustness_ratio_all_within() {
        let metrics = vec![0.99, 1.0, 0.995, 0.998, 0.991];
        let ratio = robustness_ratio(&metrics, 0.01);
        assert!((ratio - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_robustness_ratio_partial() {
        let metrics = vec![0.5, 0.8, 1.0, 0.9, 0.6];
        let ratio = robustness_ratio(&metrics, 0.05);
        assert!(ratio < 1.0); // not all within 5% of max
    }
}
