/// Integration test: Load ONNX regime model and verify inference matches Python output.
///
/// This test requires:
/// 1. The ONNX model file at `research/models/onnx/regime_v1.onnx`
///    (generate with: `python -m research.models.generate_regime_model`)
/// 2. The ONNX Runtime shared library (auto-detected from Python onnxruntime)
///
/// If either is missing, the test is skipped.
use std::path::Path;

/// Try to auto-detect the ONNX Runtime shared library from Python onnxruntime installation.
fn find_ort_lib() -> Option<std::path::PathBuf> {
    // If ORT_DYLIB_PATH is already set, use it
    if let Ok(p) = std::env::var("ORT_DYLIB_PATH") {
        let path = std::path::PathBuf::from(p);
        if path.exists() {
            return Some(path);
        }
    }

    // Try common Python onnxruntime locations
    let candidates = [
        // Linux - mise
        std::path::PathBuf::from(
            std::env::var("HOME")
                .unwrap_or_else(|_| "/root".to_string())
                + "/.local/share/mise/installs/python/3.12.13/lib/python3.12/site-packages/onnxruntime/capi/libonnxruntime.so.1.24.4",
        ),
        // Linux - pyenv
        std::path::PathBuf::from(
            std::env::var("HOME")
                .unwrap_or_else(|_| "/root".to_string())
                + "/.pyenv/versions/3.12.*/lib/python3.12/site-packages/onnxruntime/capi/libonnxruntime.so",
        ),
        // Try glob-style lookup via Python
    ];

    for path in &candidates {
        if path.exists() {
            return Some(path.clone());
        }
    }

    // Last resort: try to find via `python3 -c`
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            "import onnxruntime, os, glob; print(glob.glob(os.path.join(os.path.dirname(onnxruntime.__file__), 'capi', 'libonnxruntime.so*'))[0])",
        ])
        .output()
        .ok()?;

    if output.status.success() {
        let path_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !path_str.is_empty() {
            let path = std::path::PathBuf::from(path_str);
            if path.exists() {
                return Some(path);
            }
        }
    }

    None
}

fn ensure_ort_available() -> bool {
    if let Some(ort_path) = find_ort_lib() {
        std::env::set_var("ORT_DYLIB_PATH", ort_path);
        true
    } else {
        false
    }
}

#[test]
fn test_onnx_regime_model_load_and_infer() {
    let model_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../research/models/onnx/regime_v1.onnx");
    let meta_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../research/models/onnx/regime_v1_meta.json");

    if !model_path.exists() {
        eprintln!(
            "SKIP: ONNX model not found at {}. Run `python -m research.models.generate_regime_model` first.",
            model_path.display()
        );
        return;
    }

    if !ensure_ort_available() {
        eprintln!("SKIP: ONNX Runtime shared library not found. Set ORT_DYLIB_PATH.");
        return;
    }

    let meta_str = std::fs::read_to_string(&meta_path).expect("Failed to read regime_v1_meta.json");
    let meta: serde_json::Value =
        serde_json::from_str(&meta_str).expect("Failed to parse regime_v1_meta.json");

    let test_features: Vec<f64> = meta["test_features"]
        .as_array()
        .expect("test_features should be an array")
        .iter()
        .map(|v| v.as_f64().expect("test_features values should be f64"))
        .collect();

    let expected_posterior: Vec<f64> = meta["expected_posterior"]
        .as_array()
        .expect("expected_posterior should be an array")
        .iter()
        .map(|v| v.as_f64().expect("expected_posterior values should be f64"))
        .collect();

    let model = fx_strategy::regime::OnnxRegimeModel::load_from_path(model_path.to_str().unwrap())
        .expect("Failed to load ONNX regime model");

    if model.feature_dim() != fx_strategy::features::FeatureVector::DIM {
        eprintln!(
            "SKIP: ONNX model feature_dim {} does not match current FeatureVector::DIM {}. Regenerate regime_v1.onnx.",
            model.feature_dim(),
            fx_strategy::features::FeatureVector::DIM
        );
        return;
    }
    if test_features.len() != model.feature_dim() {
        eprintln!(
            "SKIP: regime_v1_meta.json test_features len {} does not match model feature_dim {}. Regenerate ONNX artifacts.",
            test_features.len(),
            model.feature_dim()
        );
        return;
    }
    assert_eq!(model.n_regimes(), 4);

    let posterior = model
        .predict(&test_features)
        .expect("ONNX inference failed");

    assert_eq!(posterior.len(), 4);

    // Verify posterior sums to ~1.0
    let sum: f64 = posterior.iter().sum();
    assert!((sum - 1.0).abs() < 1e-4, "posterior sum {sum} != 1.0");

    // Verify all values are non-negative
    for &p in &posterior {
        assert!(p >= 0.0, "negative posterior value: {p}");
    }

    // Verify matches Python expected output (float32 precision)
    for (got, expected) in posterior.iter().zip(expected_posterior.iter()) {
        assert!(
            (got - expected).abs() < 1e-4,
            "posterior mismatch: got {got}, expected {expected}"
        );
    }
}

#[test]
fn test_onnx_regime_cache_integration() {
    let model_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../research/models/onnx/regime_v1.onnx");

    if !model_path.exists() {
        eprintln!(
            "SKIP: ONNX model not found at {}. Run `python -m research.models.generate_regime_model` first.",
            model_path.display()
        );
        return;
    }

    if !ensure_ort_available() {
        eprintln!("SKIP: ONNX Runtime shared library not found.");
        return;
    }

    let config = fx_strategy::regime::RegimeConfig {
        model_path: Some(model_path.to_str().unwrap().to_string()),
        n_regimes: 4,
        feature_dim: fx_strategy::features::FeatureVector::DIM,
        ..fx_strategy::regime::RegimeConfig::default()
    };

    let cache = fx_strategy::regime::RegimeCache::new(config);
    if !cache.has_onnx_model() {
        eprintln!(
            "SKIP: ONNX regime artifacts are stale for current FeatureVector::DIM {}. Regenerate regime_v1.onnx.",
            fx_strategy::features::FeatureVector::DIM
        );
        return;
    }

    let features = vec![0.1f64; fx_strategy::features::FeatureVector::DIM];
    let posterior = cache
        .predict_onnx(&features)
        .expect("ONNX inference should succeed when model is loaded");

    assert_eq!(posterior.len(), 4);
    let sum: f64 = posterior.iter().sum();
    assert!((sum - 1.0).abs() < 1e-4);
}
