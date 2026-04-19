"""Verify Python research environment is correctly set up."""

import importlib


def test_core_dependencies():
    """Check that all core dependencies are importable."""
    for module in ["numpy", "pandas", "scipy", "matplotlib", "sklearn", "onnx"]:
        importlib.import_module(module)


def test_research_packages_importable():
    """Check that research subpackages are importable."""
    for pkg in [
        "research.features",
        "research.models",
        "research.backtest",
        "research.analysis",
    ]:
        importlib.import_module(pkg)


def test_onnx_export_module():
    """Check that onnx_export module is importable."""
    from research.models import onnx_export

    assert hasattr(onnx_export, "export_bayesian_lr")
    assert hasattr(onnx_export, "save_model")
