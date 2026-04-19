"""ONNX export utility for deploying trained models to Rust inference.

Python-trained models (Bayesian Linear Regression weights, HDP-HMM parameters)
are exported via ONNX for Rust-side inference using the `ort` crate.
"""

from pathlib import Path

import numpy as np
import onnx
from onnx import TensorProto, helper


def save_model(model: onnx.ModelProto, path: str | Path) -> Path:
    """Validate and save an ONNX model to disk."""
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    onnx.checker.check_model(model)
    onnx.save(model, str(path))
    return path


def export_bayesian_lr(
    feature_dim: int,
    n_actions: int,
    weights: np.ndarray,
    bias: np.ndarray | None = None,
    opset_version: int = 17,
) -> onnx.ModelProto:
    """Export Bayesian Linear Regression Q-function to ONNX.

    Args:
        feature_dim: Number of input features (phi dimension).
        n_actions: Number of actions (buy_k, sell_k, hold).
        weights: Shape (n_actions, feature_dim) weight matrix.
        bias: Optional shape (n_actions,) bias vector.
        opset_version: ONNX opset version.

    Returns:
        Validated ONNX ModelProto.
    """
    X = helper.make_tensor_value_info("features", TensorProto.FLOAT, [1, feature_dim])

    outputs = []
    for a in range(n_actions):
        outputs.append(
            helper.make_tensor_value_info(f"q_action_{a}", TensorProto.FLOAT, [1])
        )

    nodes = []
    initializers = []

    w_init = helper.make_tensor(
        f"w_global", TensorProto.FLOAT, [n_actions, feature_dim], weights.flatten().tolist()
    )
    initializers.append(w_init)

    matmul_node = helper.make_node("MatMul", ["features", "w_global"], ["q_all"], name="matmul")
    nodes.append(matmul_node)

    if bias is not None:
        b_init = helper.make_tensor("b_global", TensorProto.FLOAT, [n_actions], bias.flatten().tolist())
        initializers.append(b_init)
        add_node = helper.make_node("Add", ["q_all", "b_global"], ["q_biased"], name="add_bias")
        nodes.append(add_node)
        source = "q_biased"
    else:
        source = "q_all"

    for a in range(n_actions):
        split_node = helper.make_node(
            "Gather",
            [source, f"idx_{a}"],
            [f"q_action_{a}"],
            name=f"gather_action_{a}",
            axis=1,
        )
        idx_init = helper.make_tensor(f"idx_{a}", TensorProto.INT64, [1], [a])
        initializers.append(idx_init)
        nodes.append(split_node)

    graph = helper.make_graph(nodes, "bayesian_lr_q", [X], outputs, initializer=initializers)
    model = helper.make_model(graph, opset_imports=[helper.make_opsetid("", opset_version)])
    model.ir_version = 8

    onnx.checker.check_model(model)
    return model


def export_scalar_function(
    input_name: str,
    output_name: str,
    graph_name: str,
    transform_fn,
    input_shape: list[int],
    opset_version: int = 17,
) -> onnx.ModelProto:
    """Generic scalar function exporter for custom ONNX graphs.

    Args:
        input_name: Name of the input tensor.
        output_name: Name of the output tensor.
        graph_name: Name of the ONNX graph.
        transform_fn: Callable(nodes, inputs) -> output_name that builds the graph.
        input_shape: Shape of the input tensor.
        opset_version: ONNX opset version.

    Returns:
        ONNX ModelProto.
    """
    X = helper.make_tensor_value_info(input_name, TensorProto.FLOAT, input_shape)
    Y = helper.make_tensor_value_info(output_name, TensorProto.FLOAT, input_shape)

    nodes = []
    initializers = []
    result = transform_fn(nodes, initializers, input_name)
    assert result == output_name, f"Transform must produce {output_name}, got {result}"

    graph = helper.make_graph(nodes, graph_name, [X], [Y], initializer=initializers)
    model = helper.make_model(graph, opset_imports=[helper.make_opsetid("", opset_version)])
    model.ir_version = 8

    onnx.checker.check_model(model)
    return model
