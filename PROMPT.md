@prd.md @activity.md

We are building the project according to the PRD in this repo.

First read activity.md to see what was recently accomplished.

## Start the Application

### Rust (Core Execution Platform)
- **Build**: `cargo build`
- **Test**: `cargo test`
- **Lint**: `cargo clippy`
- **Format**: `cargo fmt --check`
- **Run**: `cargo run` (or `cargo run --bin <binary-name>` for specific binaries)

### Python (Research & ML)
- **Install**: `uv pip install -e .` or `pip install -e .`
- **Test**: `pytest research/tests/`
- **Jupyter**: `jupyter lab` (for research notebooks)

If a port is taken, try another port.

## Work on Tasks

Open prd.md and find the single highest priority task where `"passes": false`.

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

Append a dated progress entry to activity.md describing:
- What you changed
- What commands you ran
- Any issues encountered and how you resolved them

## Update Task Status

When the task is confirmed working, update that task's `"passes"` field in prd.md from `false` to `true`.

## Commit Changes

Make one git commit for that task only with a clear, descriptive message:
```
git add .
git commit -m "feat: [brief description of what was implemented]"
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

## Important Rules

- ONLY work on a SINGLE task per iteration
- Always verify with tests before marking a task as passing
- Always log your progress in activity.md
- Always commit after completing a task

## Completion

When ALL tasks have `"passes": true`, output:

<promise>COMPLETE</promise>
