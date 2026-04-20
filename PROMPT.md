@prd-design-gap.md @activity-design-gap.md @prd-backtest-fix.md @activity-backtest-fix.md @prd-verification.md @activity-verification.md @prd-forward.md @prd.md

We are implementing **design.md未実装機能** according to the PRD in `prd-design-gap.md`.
14 tasks: HDP-HMM bugs, feature interaction terms, Dynamic K, GapDetector halt, PreFailureMetrics wiring, ONNX integration, E2E pipeline.

First read `activity-design-gap.md` to see what was recently accomplished.

## Start the Application

### Rust (Core Execution Platform)
- **Build**: `cargo build`
- **Test**: `cargo test`
- **Test specific crate**: `cargo test -p fx-strategy`
- **Lint**: `cargo clippy`
- **Format**: `cargo fmt --check`
- **Run**: `cargo run --bin fx-cli`

### Python (Research & ML)
- **Install**: `uv pip install -e .` or `pip install -e .`
- **Test**: `pytest research/tests/`
- **Single test**: `pytest research/tests/test_hdp_hmm.py -v`

If a port is taken, try another port.

## Work on Tasks

Open `prd-design-gap.md` and find the single highest priority task where `"passes": false`.

Work on exactly ONE task:
1. Implement the change according to the task steps
2. Run any available checks:
   - `cargo build` (ensure compilation)
   - `cargo test` (run tests)
   - `cargo clippy` (lint)
   - `cargo fmt --check` (format check)
   - `pytest research/tests/` (Python tests, if applicable)

## Verify

After implementing, verify your work:

1. Ensure `cargo build` completes without errors
2. Run `cargo test` and confirm all tests pass
3. For Python changes, run `pytest research/tests/` if applicable
4. Check that new files are in the correct module/crate

## Log Progress

Append a dated progress entry to `activity-design-gap.md` describing:
- What you changed
- What commands you ran
- Any issues encountered and how you resolved them

## Update Task Status

When the task is confirmed working, update that task's `"passes"` field in `prd-design-gap.md` from `false` to `true`.

## Commit Changes

Make one git commit for that task only with a clear, descriptive message:
```
git add .
git commit -m "feat(gap): [brief description of what was implemented]"
```

Do NOT run `git init`, do NOT change git remotes, and do NOT push.

## Project-Specific Rules

- **Rust**: All invariant checks use `assert!` or `Result<_, RiskError>`, NEVER `debug_assert!` (removed in release builds)
- **Information leakage**: Execution-related features and `position_pnl_unrealized` MUST have enforced lag
- **OTC market**: Do NOT assume exchange-like order book behavior. Model Last-Look and Internalization
- **Hard limits**: Loss limits fire regardless of Q-values. They are checked BEFORE Q-value evaluation
- **Thompson Sampling**: σ_model is ONLY reflected through posterior sampling. NEVER include σ_model in point estimates
- **Strategy-separated rewards**: Each strategy's reward is independent. No cross-strategy reward coupling
- **ONNX**: Python-trained models are exported via ONNX for Rust-side inference
- **Paper execution**: The forward test system MUST NEVER connect to actual order pathways
- **Backward compatibility**: ONNX model is optional. When absent, fall back to heuristic regime detection

## Important Rules

- ONLY work on a SINGLE task per iteration
- Always verify with tests before marking a task as passing
- Always log your progress in `activity-design-gap.md`
- Always commit after completing a task

## Completion

When ALL tasks in `prd-design-gap.md` have `"passes": true`, output:

<promise>COMPLETE</promise>
