"""Statistical validation pipeline for backtesting."""

from .cpcv import CpcvResult, CpcvSplit, generate_cpcv_splits, run_cpcv
from .pbo import PboResult, compute_pbo
from .dsr import DsrResult, compute_dsr
from .validation import (
    SharpeCeilingResult,
    ComplexityPenaltyResult,
    LiveDegradationResult,
    check_sharpe_ceiling,
    compute_complexity_penalty,
    check_live_degradation,
)
from .leakage import LeakageTestResult, verify_information_leakage
from .sensitivity import SensitivityResult, SensitivityAnalysisResult, analyze_reward_sensitivity
from .pipeline import PipelineConfig, PipelineCheckResult, PipelineResult, run_validation_pipeline
