#!/usr/bin/env python3
"""Tkinter control plane and benchmark dashboard for the Rust blockchain simulator.

No third-party Python packages are required.
Place this file in the Cargo project root (next to Cargo.toml) and run:
    python3 control_plane.py
"""

from __future__ import annotations

import csv
import json
import queue
import random
import socket
import statistics
import subprocess
import threading
import time
from collections import deque
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
import tkinter as tk
from tkinter import filedialog, messagebox, ttk
from typing import Dict, Optional, Tuple


Address = Tuple[str, int]


@dataclass
class NodeRuntime:
    node_id: int
    process: subprocess.Popen
    address: Optional[Address] = None
    paused_until: float = 0.0
    status: Optional[dict] = None
    last_sample: Optional[Tuple[float, int, int]] = None  # ts, committed tx, blocks
    tx_rate: float = 0.0
    block_rate: float = 0.0


@dataclass
class SimulationState:
    nodes: Dict[int, NodeRuntime] = field(default_factory=dict)
    running: bool = False
    load_running: bool = False
    tx_counter: int = 0
    submitted: int = 0
    submit_times: deque = field(default_factory=lambda: deque(maxlen=10000))
    metrics_history: list = field(default_factory=list)


class ControlPlane:
    def __init__(self, root: tk.Tk):
        self.root = root
        self.root.title("Rust BFT Control Plane")
        self.root.geometry("1420x850")
        self.state = SimulationState()
        self.ui_queue: queue.Queue = queue.Queue()
        self.stop_event = threading.Event()
        self.load_stop_event = threading.Event()
        self.poll_thread: Optional[threading.Thread] = None
        self.bootstrap_listener: Optional[socket.socket] = None
        self.run_config = {}

        self.project_dir = tk.StringVar(value=str(Path.cwd()))
        self.scenario_var = tk.StringVar()
        self.available_scenarios = []
        self.current_scenario = None
        self.current_run_dir = None
        self.scenario_running = False
        self.scenario_start_time = None
        self.metrics_history = []
        self.scenario_events = []
        self.scenario_stop_event = threading.Event()
        self.scenario_data_lock = threading.Lock()
        self.load_settings_lock = threading.Lock()
        self.load_wakeup_event = threading.Event()
        self.load_rate = 20.0
        self.load_target = "random"
        self.binary_path = tk.StringVar(value="target/debug/light_dex")
        self.node_count = tk.IntVar(value=7)
        self.block_time_ms = tk.IntVar(value=500)
        self.timeout_ms = tk.IntVar(value=1500)
        self.poll_ms = tk.IntVar(value=500)
        self.tx_rate = tk.DoubleVar(value=20.0)
        self.burst_count = tk.IntVar(value=100)
        self.pause_ms = tk.IntVar(value=5000)
        self.tx_payload = tk.StringVar(value="manual transaction")
        self.target_node = tk.StringVar(value="random")
        self.fault_target_node = tk.StringVar(value="random")
        self.build_before_launch = tk.BooleanVar(value=True)
        self.verbose_logs = tk.BooleanVar(value=False)

        self.metric_vars = {
            "alive": tk.StringVar(value="0"),
            "submitted": tk.StringVar(value="0"),
            "submit_tps": tk.StringVar(value="0.0"),
            "commit_tps": tk.StringVar(value="0.0"),
            "blocks_s": tk.StringVar(value="0.0"),
            "latency": tk.StringVar(value="0.0 ms"),
            "height": tk.StringVar(value="-"),
            "view": tk.StringVar(value="-"),
            "primary": tk.StringVar(value="-"),
            "consistency": tk.StringVar(value="unknown"),
        }

        self._build_ui()
        self.refresh_scenarios()
        self.root.after(100, self._drain_ui_queue)
        self.root.protocol("WM_DELETE_WINDOW", self._on_close)

    # ---------- UI ----------
    def _build_ui(self):
        outer = ttk.Panedwindow(self.root, orient=tk.VERTICAL)
        outer.pack(fill=tk.BOTH, expand=True)

        top = ttk.Frame(outer, padding=8)
        middle = ttk.Frame(outer, padding=8)
        bottom = ttk.Frame(outer, padding=8)
        outer.add(top, weight=0)
        outer.add(middle, weight=3)
        outer.add(bottom, weight=2)

        config = ttk.LabelFrame(top, text="Simulation configuration", padding=8)
        config.pack(fill=tk.X)

        entries = [
            ("Project dir", self.project_dir, 32),
            ("Binary", self.binary_path, 24),
            ("Nodes", self.node_count, 7),
            ("Block ms", self.block_time_ms, 8),
            ("Timeout ms", self.timeout_ms, 9),
            ("Poll ms", self.poll_ms, 8),
        ]
        for col, (label, var, width) in enumerate(entries):
            ttk.Label(config, text=label).grid(row=0, column=col, sticky="w", padx=3)
            ttk.Entry(config, textvariable=var, width=width).grid(row=1, column=col, sticky="ew", padx=3)

        ttk.Checkbutton(config, text="cargo build before launch", variable=self.build_before_launch).grid(
            row=2, column=0, columnspan=2, sticky="w", pady=(6, 0)
        )
        ttk.Checkbutton(config, text="verbose child logs", variable=self.verbose_logs).grid(
            row=2, column=2, columnspan=2, sticky="w", pady=(6, 0)
        )
        ttk.Button(config, text="Launch", command=self.launch).grid(row=2, column=4, padx=4, pady=(6, 0))
        ttk.Button(config, text="Stop all", command=self.stop_all).grid(row=2, column=5, padx=4, pady=(6, 0))
        ttk.Button(config, text="Export CSV", command=self.export_csv).grid(row=2, column=6, padx=4, pady=(6, 0))

        scenario_frame = ttk.LabelFrame(top, text="Scenario", padding=8)
        scenario_frame.pack(fill=tk.X, pady=(8, 0))
        self.scenario_combo = ttk.Combobox(
            scenario_frame,
            textvariable=self.scenario_var,
            state="readonly",
            width=24,
        )
        self.scenario_combo.pack(side=tk.LEFT, padx=(0, 5))
        self.scenario_combo.bind("<<ComboboxSelected>>", self._show_scenario_description)
        ttk.Button(scenario_frame, text="Refresh", command=self.refresh_scenarios).pack(side=tk.LEFT, padx=5)
        self.run_scenario_button = ttk.Button(
            scenario_frame,
            text="Run Scenario",
            command=self.run_selected_scenario,
        )
        self.run_scenario_button.pack(side=tk.LEFT, padx=5)
        self.scenario_description = ttk.Label(scenario_frame, text="")
        self.scenario_description.pack(side=tk.LEFT, padx=(12, 0))

        controls = ttk.Frame(top)
        controls.pack(fill=tk.X, pady=(8, 0))
        controls.columnconfigure(0, weight=3)
        controls.columnconfigure(1, weight=2)

        traffic = ttk.LabelFrame(controls, text="Traffic", padding=8)
        traffic.grid(row=0, column=0, sticky="nsew", padx=(0, 4))
        ttk.Label(traffic, text="Tx target").grid(row=0, column=0, sticky="w")
        self.target_combo = ttk.Combobox(
            traffic, textvariable=self.target_node, values=["random"], width=9, state="readonly"
        )
        self.target_combo.grid(row=1, column=0, padx=3)
        ttk.Label(traffic, text="Payload").grid(row=0, column=1, sticky="w")
        ttk.Entry(traffic, textvariable=self.tx_payload, width=28).grid(row=1, column=1, padx=3)
        ttk.Button(traffic, text="Send tx", command=self.send_manual_tx).grid(row=1, column=2, padx=3)
        ttk.Label(traffic, text="Burst count").grid(row=0, column=3, sticky="w")
        ttk.Entry(traffic, textvariable=self.burst_count, width=8).grid(row=1, column=3, padx=3)
        ttk.Button(traffic, text="Send burst", command=self.send_burst).grid(row=1, column=4, padx=3)
        ttk.Label(traffic, text="Load tx/s").grid(row=0, column=5, sticky="w")
        load_rate_entry = ttk.Entry(traffic, textvariable=self.tx_rate, width=8)
        load_rate_entry.grid(row=1, column=5, padx=3)
        load_rate_entry.bind("<Return>", lambda _event: self.start_load())
        ttk.Button(traffic, text="Start / update", command=self.start_load).grid(row=1, column=6, padx=3)
        ttk.Button(traffic, text="Stop load", command=self.stop_load).grid(row=1, column=7, padx=3)

        faults = ttk.LabelFrame(controls, text="Faults", padding=8)
        faults.grid(row=0, column=1, sticky="nsew", padx=(4, 0))
        ttk.Label(faults, text="Fault target").grid(row=0, column=0, sticky="w")
        self.fault_target_combo = ttk.Combobox(
            faults, textvariable=self.fault_target_node, values=["random"], width=9, state="readonly"
        )
        self.fault_target_combo.grid(row=1, column=0, padx=3)
        ttk.Label(faults, text="Pause ms").grid(row=0, column=1, sticky="w")
        ttk.Entry(faults, textvariable=self.pause_ms, width=8).grid(row=1, column=1, padx=3)
        ttk.Button(faults, text="Pause", command=self.pause_target).grid(row=1, column=2, padx=3)
        ttk.Button(faults, text="Kill", command=self.kill_target).grid(row=1, column=3, padx=3)
        ttk.Button(faults, text="Random pause", command=lambda: self.pause_target(force_random=True)).grid(row=1, column=4, padx=3)
        ttk.Button(faults, text="Random kill", command=lambda: self.kill_target(force_random=True)).grid(row=1, column=5, padx=3)

        metrics = ttk.LabelFrame(middle, text="Benchmark", padding=8)
        metrics.pack(fill=tk.X)
        metric_order = [
            ("Alive", "alive"),
            ("Submitted", "submitted"),
            ("Submit TPS", "submit_tps"),
            ("Committed TPS", "commit_tps"),
            ("Blocks/s", "blocks_s"),
            ("Commit latency", "latency"),
            ("Height min/max", "height"),
            ("View", "view"),
            ("Primary", "primary"),
            ("Chain", "consistency"),
        ]
        for i, (label, key) in enumerate(metric_order):
            box = ttk.Frame(metrics, padding=(8, 3))
            box.grid(row=0, column=i, sticky="nsew")
            ttk.Label(box, text=label).pack()
            ttk.Label(box, textvariable=self.metric_vars[key], font=("TkDefaultFont", 11, "bold")).pack()
            metrics.columnconfigure(i, weight=1)

        columns = (
            "node", "pid", "state", "peers", "view", "primary", "height", "seq",
            "mempool", "rx", "committed", "blocks", "tps", "bps", "latency"
        )
        self.tree = ttk.Treeview(middle, columns=columns, show="headings", height=19)
        headings = {
            "node": "Node", "pid": "PID", "state": "State", "peers": "Peers",
            "view": "View", "primary": "Primary", "height": "Height", "seq": "Seq",
            "mempool": "Mempool", "rx": "Tx seen", "committed": "Tx committed",
            "blocks": "Blocks", "tps": "TPS", "bps": "Blocks/s", "latency": "Avg latency ms"
        }
        widths = {"node": 55, "pid": 75, "state": 100, "peers": 70, "view": 55, "primary": 65,
                  "height": 65, "seq": 55, "mempool": 75, "rx": 75, "committed": 95,
                  "blocks": 65, "tps": 65, "bps": 70, "latency": 100}
        for col in columns:
            self.tree.heading(col, text=headings[col])
            self.tree.column(col, width=widths[col], anchor=tk.CENTER)
        self.tree.pack(fill=tk.BOTH, expand=True, pady=(8, 0))
        self.tree.bind("<<TreeviewSelect>>", self._show_selected_chain)

        chain_frame = ttk.LabelFrame(bottom, text="Selected node — recent committed blocks", padding=6)
        chain_frame.pack(side=tk.LEFT, fill=tk.BOTH, expand=True)
        self.chain_text = tk.Text(chain_frame, height=10, wrap=tk.NONE)
        self.chain_text.pack(fill=tk.BOTH, expand=True)

        log_frame = ttk.LabelFrame(bottom, text="Controller / node events", padding=6)
        log_frame.pack(side=tk.LEFT, fill=tk.BOTH, expand=True, padx=(8, 0))
        self.log_text = tk.Text(log_frame, height=10, wrap=tk.WORD)
        self.log_text.pack(fill=tk.BOTH, expand=True)

    # ---------- launch / bootstrap ----------
    def launch(self):
        if self.scenario_running:
            messagebox.showinfo("Scenario running", "Wait for the scenario to finish or stop it first.")
            return
        if self.state.running:
            messagebox.showinfo("Already running", "Stop the current simulation first.")
            return
        try:
            config = {
                "project_dir": self.project_dir.get(),
                "binary_path": self.binary_path.get(),
                "node_count": int(self.node_count.get()),
                "block_time_ms": int(self.block_time_ms.get()),
                "timeout_ms": int(self.timeout_ms.get()),
                "poll_ms": int(self.poll_ms.get()),
                "build": bool(self.build_before_launch.get()),
                "verbose_logs": bool(self.verbose_logs.get()),
            }
        except Exception as exc:
            messagebox.showerror("Invalid config", str(exc))
            return
        self.run_config = config
        self.stop_event.clear()
        self.load_stop_event.set()
        threading.Thread(target=self._launch_worker, args=(config,), daemon=True).start()

    def _scenario_paths(self):
        project = Path(self.project_dir.get()).expanduser().resolve()
        return project / "scenarios", project / "benchmark_results"

    def load_scenarios(self, scenario_dir: Path):
        scenarios = []

        for path in sorted(scenario_dir.glob("*.json")):
            try:
                with path.open("r", encoding="utf-8") as f:
                    scenario = json.load(f)
                self._validate_scenario(scenario, path.name)
                scenarios.append({
                    "path": path,
                    "name": scenario.get("name", path.stem),
                    "description": scenario.get("description", ""),
                })

            except Exception as exc:
                self._log(
                    f"Failed loading scenario {path.name}: {exc}"
                )

        return scenarios

    @staticmethod
    def _validate_scenario(scenario: dict, source: str = "scenario"):
        if not isinstance(scenario, dict):
            raise ValueError(f"{source}: root must be a JSON object")
        for key in ("name", "duration_seconds", "network"):
            if key not in scenario:
                raise ValueError(f"{source}: missing required field {key!r}")
        if float(scenario["duration_seconds"]) <= 0:
            raise ValueError(f"{source}: duration_seconds must be positive")
        network = scenario["network"]
        for key in ("nodes", "block_time_ms", "consensus_timeout_ms"):
            if key not in network or int(network[key]) <= 0:
                raise ValueError(f"{source}: network.{key} must be positive")
        traffic_type = scenario.get("traffic", {}).get("type", "steady")
        if traffic_type not in {"steady", "burst", "ramp"}:
            raise ValueError(f"{source}: unknown traffic type {traffic_type!r}")
        for fault in scenario.get("faults", []):
            if "at_seconds" not in fault or "action" not in fault:
                raise ValueError(f"{source}: every fault needs at_seconds and action")
            if float(fault["at_seconds"]) < 0:
                raise ValueError(f"{source}: fault at_seconds cannot be negative")

    def refresh_scenarios(self):
        try:
            scenario_dir, results_dir = self._scenario_paths()
            scenario_dir.mkdir(parents=True, exist_ok=True)
            results_dir.mkdir(parents=True, exist_ok=True)
            self.available_scenarios = self.load_scenarios(scenario_dir)
        except Exception as exc:
            self.available_scenarios = []
            self._log(f"Failed refreshing scenarios: {exc}")

        names = [scenario["name"] for scenario in self.available_scenarios]
        self.scenario_combo.configure(values=names)
        if names:
            current = self.scenario_var.get()
            self.scenario_combo.current(names.index(current) if current in names else 0)
        else:
            self.scenario_var.set("")
        self._show_scenario_description()

    def _show_scenario_description(self, *_):
        selected = self.scenario_var.get()
        description = next(
            (item["description"] for item in self.available_scenarios if item["name"] == selected),
            "No scenarios found",
        )
        self.scenario_description.configure(text=description)

    def get_selected_scenario(self):
        selected = self.scenario_var.get()
        for info in self.available_scenarios:
            if info["name"] == selected:
                with info["path"].open("r", encoding="utf-8") as f:
                    scenario = json.load(f)
                self._validate_scenario(scenario, info["path"].name)
                return scenario
        raise RuntimeError(f"Scenario not found: {selected or '<none>'}")

    def _launch_worker(self, config):
        try:
            project = Path(config["project_dir"]).expanduser().resolve()
            binary = Path(config["binary_path"])
            if not binary.is_absolute():
                binary = project / binary

            if config["build"]:
                self._log("Running cargo build …")
                result = subprocess.run(["cargo", "build"], cwd=project, capture_output=True, text=True)
                if result.returncode != 0:
                    self._log(result.stdout)
                    self._log(result.stderr)
                    raise RuntimeError("cargo build failed")

            if self.scenario_running and self.scenario_stop_event.is_set():
                raise RuntimeError("Scenario launch cancelled")

            count = config["node_count"]
            if count < 1:
                raise ValueError("node count must be >= 1")

            parent = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            parent.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            parent.bind(("127.0.0.1", 0))
            parent.listen(max(128, count * 2))
            parent.settimeout(30.0)
            self.bootstrap_listener = parent
            parent_addr = parent.getsockname()
            parent_text = f"{parent_addr[0]}:{parent_addr[1]}"
            self._log(f"Bootstrap listening at {parent_text}")

            self.state = SimulationState(running=False)
            for node_id in range(count):
                if self.scenario_running and self.scenario_stop_event.is_set():
                    raise RuntimeError("Scenario launch cancelled")
                cmd = [
                    str(binary),
                    "--node", str(node_id),
                    "--parent", parent_text,
                    "--block-time-ms", str(config["block_time_ms"]),
                    "--timeout-ms", str(config["timeout_ms"]),
                ]
                proc = subprocess.Popen(
                    cmd,
                    cwd=project,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    text=True,
                    bufsize=1,
                )
                self.state.nodes[node_id] = NodeRuntime(node_id=node_id, process=proc)
                threading.Thread(
                    target=self._drain_process_output,
                    args=(node_id, proc, config["verbose_logs"]),
                    daemon=True,
                ).start()

            # Phase 1: registrations.
            addresses: Dict[int, Address] = {}
            while len(addresses) < count:
                conn, _ = parent.accept()
                with conn:
                    line = self._recv_line(conn)
                parts = line.strip().split()
                if len(parts) == 3 and parts[0] == "REGISTER":
                    node_id = int(parts[1])
                    host, port = parts[2].rsplit(":", 1)
                    addresses[node_id] = (host, int(port))
                    self.state.nodes[node_id].address = addresses[node_id]
                    self._log(f"Registered node {node_id} at {parts[2]}")

            # Phase 2: distribute peer table.
            for node_id, addr in addresses.items():
                with socket.create_connection(addr, timeout=5.0) as sock:
                    for peer_id, peer_addr in addresses.items():
                        if peer_id == node_id:
                            continue
                        sock.sendall(f"REGISTER {peer_id} {peer_addr[0]}:{peer_addr[1]}\n".encode())
                    sock.sendall(b"DONE\n")

            # Phase 3: barrier.
            ready_sockets = []
            ready_ids = set()
            while len(ready_ids) < count:
                conn, _ = parent.accept()
                line = self._recv_line(conn)
                parts = line.strip().split()
                if len(parts) == 2 and parts[0] == "READY":
                    node_id = int(parts[1])
                    ready_ids.add(node_id)
                    ready_sockets.append(conn)
                    self._log(f"Node {node_id} ready")
                else:
                    conn.close()

            for conn in ready_sockets:
                try:
                    conn.sendall(b"START\n")
                finally:
                    conn.close()
            parent.close()
            self.bootstrap_listener = None

            self.state.running = True
            self._ui(lambda: self._configure_target_choices(count))
            self._log(f"Simulation started with {count} nodes")
            self.poll_thread = threading.Thread(target=self._poll_loop, daemon=True)
            self.poll_thread.start()
        except Exception as exc:
            self._log(f"Launch failed: {exc}")
            self._stop_all_worker(force=True)

    def _drain_process_output(self, node_id, proc, verbose):
        if not proc.stdout:
            return

        for line in proc.stdout:
            line = line.rstrip()

            if line:
                self._log(f"[node {node_id}] {line}")
    # ---------- protocol helpers ----------
    @staticmethod
    def _recv_line(sock: socket.socket, max_bytes: int = 1_000_000) -> str:
        data = bytearray()
        while len(data) < max_bytes:
            chunk = sock.recv(4096)
            if not chunk:
                break
            data.extend(chunk)
            if b"\n" in chunk:
                break
        return data.split(b"\n", 1)[0].decode("utf-8", errors="replace")

    @staticmethod
    def _send_json(addr: Address, obj: dict, expect_reply: bool = False, timeout: float = 0.75):
        with socket.create_connection(addr, timeout=timeout) as sock:
            sock.settimeout(timeout)
            sock.sendall((json.dumps(obj, separators=(",", ":")) + "\n").encode())
            if expect_reply:
                line = ControlPlane._recv_line(sock)
                if not line:
                    raise ConnectionError("empty response")
                return json.loads(line)
        return None

    def _status_for_node(self, node_id: int, runtime: NodeRuntime):
        if runtime.process.poll() is not None or runtime.address is None:
            return node_id, None, "dead"
        try:
            status = self._send_json(runtime.address, {"Control": "Status"}, expect_reply=True, timeout=0.4)
            return node_id, status, "ok"
        except Exception:
            state = "paused" if time.monotonic() < runtime.paused_until else "unresponsive"
            return node_id, None, state

    # ---------- polling / benchmark ----------
    def _poll_loop(self):
        while self.state.running and not self.stop_event.is_set():
            started = time.monotonic()
            runtimes = list(self.state.nodes.items())
            workers = min(32, max(1, len(runtimes)))
            results = []
            with ThreadPoolExecutor(max_workers=workers) as pool:
                futures = [pool.submit(self._status_for_node, node_id, runtime) for node_id, runtime in runtimes]
                for fut in as_completed(futures):
                    results.append(fut.result())

            now = time.monotonic()
            statuses = []
            for node_id, status, state_name in results:
                runtime = self.state.nodes[node_id]
                if status is not None:
                    runtime.status = status
                    committed = int(status.get("transactions_committed", 0))
                    blocks = int(status.get("blocks_committed", 0))
                    if runtime.last_sample is not None:
                        prev_t, prev_tx, prev_blocks = runtime.last_sample
                        dt = max(1e-9, now - prev_t)
                        runtime.tx_rate = max(0.0, committed - prev_tx) / dt
                        runtime.block_rate = max(0.0, blocks - prev_blocks) / dt
                    runtime.last_sample = (now, committed, blocks)
                    statuses.append(status)
                self._ui(lambda nid=node_id, st=status, sn=state_name: self._update_tree_row(nid, st, sn))

            self._update_aggregate_metrics(statuses, now)
            self._ui(self._show_selected_chain)

            interval = max(0.1, self.run_config.get("poll_ms", 500) / 1000.0)
            elapsed = time.monotonic() - started
            self.stop_event.wait(max(0.01, interval - elapsed))

    def _update_aggregate_metrics(self, statuses: list, now: float):
        # Rolling client submission rate over the last 5 seconds.
        while self.state.submit_times and now - self.state.submit_times[0] > 5.0:
            self.state.submit_times.popleft()
        submit_tps = len(self.state.submit_times) / 5.0

        responsive_ids = [int(s["node_id"]) for s in statuses]
        rates = [self.state.nodes[i].tx_rate for i in responsive_ids if self.state.nodes[i].last_sample]
        block_rates = [self.state.nodes[i].block_rate for i in responsive_ids if self.state.nodes[i].last_sample]
        commit_tps = statistics.median(rates) if rates else 0.0
        blocks_s = statistics.median(block_rates) if block_rates else 0.0
        latencies = [float(s.get("average_commit_latency_ms", 0.0)) for s in statuses if s.get("average_commit_latency_ms", 0.0) > 0]
        latency = statistics.median(latencies) if latencies else 0.0
        heights = [int(s.get("chain_height", 0)) for s in statuses]
        views = sorted({int(s.get("current_view", 0)) for s in statuses})
        primaries = sorted({int(s.get("current_primary", 0)) for s in statuses})

        tips = {(int(s.get("chain_height", 0)), self._tip_digest(s)) for s in statuses}
        consistency = "consistent" if statuses and len(tips) == 1 else ("split" if statuses else "unknown")

        alive = sum(1 for r in self.state.nodes.values() if r.process.poll() is None)
        values = {
            "alive": str(alive),
            "submitted": str(self.state.submitted),
            "submit_tps": f"{submit_tps:.1f}",
            "commit_tps": f"{commit_tps:.1f}",
            "blocks_s": f"{blocks_s:.2f}",
            "latency": f"{latency:.1f} ms",
            "height": f"{min(heights)}/{max(heights)}" if heights else "-",
            "view": ",".join(map(str, views)) if views else "-",
            "primary": ",".join(map(str, primaries)) if primaries else "-",
            "consistency": consistency,
        }
        for key, val in values.items():
            self._ui(lambda k=key, v=val: self.metric_vars[k].set(v))

        self.state.metrics_history.append({
            "timestamp": time.time(),
            "responsive_nodes": len(statuses),
            "alive_processes": alive,
            "submitted_total": self.state.submitted,
            "submit_tps": submit_tps,
            "committed_tps_median": commit_tps,
            "blocks_per_second_median": blocks_s,
            "commit_latency_ms_median": latency,
            "height_min": min(heights) if heights else "",
            "height_max": max(heights) if heights else "",
            "views": ";".join(map(str, views)),
            "primaries": ";".join(map(str, primaries)),
            "chain_consistency": consistency,
        })
        self.record_metrics_sample(
            commit_tps=commit_tps,
            submit_tps=submit_tps,
            block_rate=blocks_s,
            median_latency=latency if latencies else None,
            statuses=statuses,
        )

    def record_metrics_sample(
        self,
        commit_tps: float,
        submit_tps: float,
        block_rate: float,
        median_latency: Optional[float],
        statuses: list,
    ):
        if not self.scenario_running or self.scenario_start_time is None:
            return

        heights = [int(status.get("chain_height", 0)) for status in statuses]
        mempools = [int(status.get("mempool_size", 0)) for status in statuses]
        views = [int(status.get("current_view", 0)) for status in statuses]
        primaries = [int(status.get("current_primary", 0)) for status in statuses]
        sample = {
            "elapsed_seconds": time.monotonic() - self.scenario_start_time,
            "submit_tps": submit_tps,
            "commit_tps": commit_tps,
            "blocks_per_second": block_rate,
            "commit_latency_ms": median_latency,
            "responsive_nodes": len(statuses),
            "height_min": min(heights) if heights else 0,
            "height_max": max(heights) if heights else 0,
            "mempool_total": sum(mempools),
            "view": max(views) if views else 0,
            "primary": max(primaries) if primaries else 0,
        }
        with self.scenario_data_lock:
            if not self.scenario_running:
                return
            self.metrics_history.append(sample)

    @staticmethod
    def _tip_digest(status: dict) -> str:
        blocks = status.get("recent_blocks") or []
        return blocks[0].get("digest", "") if blocks else ""

    def _update_tree_row(self, node_id: int, status: Optional[dict], state_name: str):
        runtime = self.state.nodes.get(node_id)
        pid = runtime.process.pid if runtime else "-"
        if status:
            vals = (
                node_id, pid, "ok", f"{status['peer_count']}/{status['expected_peers']}",
                status["current_view"], status["current_primary"], status["chain_height"],
                status.get("latest_sequence"), status["mempool_size"], status["transactions_received"],
                status["transactions_committed"], status["blocks_committed"],
                f"{runtime.tx_rate:.1f}", f"{runtime.block_rate:.2f}",
                f"{status.get('average_commit_latency_ms', 0.0):.1f}",
            )
        else:
            vals = (node_id, pid, state_name, "-", "-", "-", "-", "-", "-", "-", "-", "-", "-", "-", "-")
        iid = str(node_id)
        if self.tree.exists(iid):
            self.tree.item(iid, values=vals)
        else:
            self.tree.insert("", "end", iid=iid, values=vals)

    def _show_selected_chain(self, *_):
        selected = self.tree.selection()
        if not selected:
            return
        node_id = int(selected[0])
        runtime = self.state.nodes.get(node_id)
        status = runtime.status if runtime else None
        self.chain_text.delete("1.0", tk.END)
        if not status:
            self.chain_text.insert(tk.END, "No status available.\n")
            return
        blocks = status.get("recent_blocks") or []
        if not blocks:
            self.chain_text.insert(tk.END, "No committed blocks yet.\n")
            return
        for block in blocks:
            self.chain_text.insert(
                tk.END,
                f"seq={block['sequence']:>5}  txs={block['transaction_count']:>5}  hash={block['digest']}\n",
            )

    # ---------- transactions ----------
    def _alive_node_ids(self):
        return [node_id for node_id, r in self.state.nodes.items() if r.process.poll() is None and r.address is not None]

    def _configure_target_choices(self, count: int):
        values = ["random"] + [str(i) for i in range(count)]
        self.target_combo.configure(values=values)
        self.fault_target_combo.configure(values=values)

    def _choose_target_value(self, value: str, force_random=False) -> Optional[int]:
        alive = self._alive_node_ids()
        if not alive:
            return None
        if force_random or value == "random":
            return random.choice(alive)
        try:
            target = int(value)
        except ValueError:
            return random.choice(alive)
        return target if target in alive else random.choice(alive)

    def _choose_target(self, force_random=False, for_fault=False) -> Optional[int]:
        value = self.fault_target_node.get() if for_fault else self.target_node.get()
        return self._choose_target_value(value, force_random=force_random)

    def _submit_transaction(self, payload: str, target: Optional[int] = None):
        if target is None:
            target = self._choose_target()
        if target is None:
            return False
        runtime = self.state.nodes[target]
        try:
            self._send_json(runtime.address, {"Transaction": {"payload": payload}}, timeout=0.5)
            self.state.submitted += 1
            self.state.submit_times.append(time.monotonic())
            return True
        except Exception as exc:
            self._log(f"Transaction to node {target} failed: {exc}")
            return False

    def send_random_transaction(self, traffic: Optional[dict] = None):
        traffic = traffic or {}
        target_setting = traffic.get("target", "random")
        alive = self._alive_node_ids()
        if not alive:
            return False
        if target_setting == "random":
            target = random.choice(alive)
        else:
            try:
                requested = int(target_setting)
            except (TypeError, ValueError):
                requested = -1
            target = requested if requested in alive else random.choice(alive)

        self.state.tx_counter += 1
        return self._submit_transaction(f"scenario tx #{self.state.tx_counter}", target=target)

    def send_manual_tx(self):
        payload = self.tx_payload.get().strip() or "manual transaction"
        self.state.tx_counter += 1
        self._submit_transaction(f"{payload} #{self.state.tx_counter}")

    def send_burst(self):
        if not self.state.running:
            return
        count = max(1, int(self.burst_count.get()))
        target = self.target_node.get()
        threading.Thread(target=self._burst_worker, args=(count, target), daemon=True).start()

    def _burst_worker(self, count: int, target_value: str):
        for _ in range(count):
            if not self.state.running:
                break
            self.state.tx_counter += 1
            target = self._choose_target_value(target_value)
            if target is None:
                continue
            self._submit_transaction(f"burst tx #{self.state.tx_counter}", target=target)

    def start_load(self):
        if not self.state.running:
            return
        try:
            rate = max(0.1, float(self.tx_rate.get()))
        except Exception:
            rate = 1.0
        with self.load_settings_lock:
            self.load_rate = rate
            self.load_target = self.target_node.get()
        if self.state.load_running:
            self.load_wakeup_event.set()
            self._log(f"Load generator updated to {rate:.1f} tx/s")
            return
        self.state.load_running = True
        self.load_stop_event.clear()
        self.load_wakeup_event.clear()
        threading.Thread(target=self._load_worker, daemon=True).start()
        self._log(f"Load generator started at {rate:.1f} tx/s")

    def stop_load(self):
        self.state.load_running = False
        self.load_stop_event.set()
        self.load_wakeup_event.set()
        self._log("Load generator stopped")

    def _load_worker(self):
        next_send = time.monotonic()
        while self.state.running and not self.load_stop_event.is_set():
            with self.load_settings_lock:
                rate = self.load_rate
                target_value = self.load_target
            period = 1.0 / rate
            now = time.monotonic()
            if now < next_send:
                if self.load_wakeup_event.wait(next_send - now):
                    self.load_wakeup_event.clear()
                    next_send = time.monotonic()
                continue
            self.state.tx_counter += 1
            target = self._choose_target_value(target_value)
            if target is None:
                self.load_stop_event.wait(min(period, 0.1))
                continue
            self._submit_transaction(f"load tx #{self.state.tx_counter}", target=target)
            next_send = max(next_send + period, time.monotonic())
        self.state.load_running = False

    def run_steady_traffic(self, traffic: dict, stop_event: threading.Event):
        tps = max(0.1, float(traffic.get("tps", 10)))
        interval = 1.0 / tps
        next_send = time.monotonic()
        while self.state.running and not stop_event.is_set():
            self.send_random_transaction(traffic)
            next_send += interval
            delay = next_send - time.monotonic()
            if delay > 0:
                stop_event.wait(delay)

    def run_burst_traffic(self, traffic: dict, stop_event: threading.Event):
        burst_size = max(1, int(traffic.get("burst_size", 100)))
        every_seconds = max(0.01, float(traffic.get("every_seconds", 5)))
        while self.state.running and not stop_event.is_set():
            for _ in range(burst_size):
                if not self.state.running or stop_event.is_set():
                    return
                self.send_random_transaction(traffic)
            stop_event.wait(every_seconds)

    def run_ramp_traffic(self, traffic: dict, stop_event: threading.Event):
        current_tps = max(0.1, float(traffic.get("start_tps", 10)))
        increase = float(traffic.get("increase_tps", 10))
        every_seconds = max(0.01, float(traffic.get("every_seconds", 5)))
        max_tps = max(current_tps, float(traffic.get("max_tps", 1000)))
        next_ramp = time.monotonic() + every_seconds
        next_send = time.monotonic()

        while self.state.running and not stop_event.is_set():
            now = time.monotonic()
            if now >= next_ramp:
                current_tps = min(current_tps + increase, max_tps)
                self.log_scenario_event("traffic_ramp", {"tps": current_tps})
                while next_ramp <= now:
                    next_ramp += every_seconds

            self.send_random_transaction(traffic)
            next_send += 1.0 / current_tps
            delay = next_send - time.monotonic()
            if delay > 0:
                stop_event.wait(delay)

    # ---------- scenarios ----------
    def run_selected_scenario(self):
        if self.scenario_running:
            self._log("A scenario is already running")
            return
        if self.state.running:
            self._log("Stop the current simulation before running a scenario")
            return

        try:
            scenario = self.get_selected_scenario()
            network = scenario["network"]
            self.node_count.set(int(network["nodes"]))
            self.block_time_ms.set(int(network["block_time_ms"]))
            self.timeout_ms.set(int(network["consensus_timeout_ms"]))
            _, results_dir = self._scenario_paths()
            results_dir.mkdir(parents=True, exist_ok=True)
            config = {
                "project_dir": self.project_dir.get(),
                "binary_path": self.binary_path.get(),
                "node_count": int(network["nodes"]),
                "block_time_ms": int(network["block_time_ms"]),
                "timeout_ms": int(network["consensus_timeout_ms"]),
                "poll_ms": int(self.poll_ms.get()),
                "build": bool(self.build_before_launch.get()),
                "verbose_logs": bool(self.verbose_logs.get()),
            }
        except Exception as exc:
            self._log(f"Cannot run scenario: {exc}")
            return

        self.scenario_running = True
        self.scenario_stop_event.clear()
        self.run_scenario_button.configure(state=tk.DISABLED)
        threading.Thread(
            target=self._run_scenario,
            args=(scenario, config, results_dir),
            daemon=True,
        ).start()

    def _run_scenario(self, scenario: dict, config: dict, results_dir: Path):
        traffic_thread = None
        stop_traffic = threading.Event()
        try:
            self.current_scenario = scenario
            self.scenario_start_time = None
            self.begin_scenario_logging(scenario, results_dir)
            self.run_config = config
            self.stop_event.clear()
            self.load_stop_event.set()
            self._launch_worker(config)
            if not self.state.running:
                raise RuntimeError("Simulation did not start")

            if self.scenario_stop_event.wait(1.0):
                raise RuntimeError("Scenario stopped during cluster startup")

            self.scenario_start_time = time.monotonic()
            self.log_scenario_event("traffic_started", {"type": scenario.get("traffic", {}).get("type", "steady")})

            traffic = scenario.get("traffic", {})
            traffic_type = traffic.get("type", "steady")
            traffic_targets = {
                "steady": self.run_steady_traffic,
                "burst": self.run_burst_traffic,
                "ramp": self.run_ramp_traffic,
            }
            target = traffic_targets[traffic_type]
            traffic_thread = threading.Thread(target=target, args=(traffic, stop_traffic), daemon=True)
            traffic_thread.start()

            faults = sorted(scenario.get("faults", []), key=lambda fault: float(fault["at_seconds"]))
            duration = float(scenario["duration_seconds"])
            next_fault = 0
            while not self.scenario_stop_event.is_set():
                elapsed = time.monotonic() - self.scenario_start_time
                if elapsed >= duration:
                    break
                while next_fault < len(faults) and elapsed >= float(faults[next_fault]["at_seconds"]):
                    self.execute_fault(faults[next_fault])
                    next_fault += 1
                self.scenario_stop_event.wait(0.05)

            stop_traffic.set()
            if self.scenario_stop_event.is_set():
                self.log_scenario_event("scenario_stopped")
            else:
                self.log_scenario_event("scenario_finished")
        except Exception as exc:
            stop_traffic.set()
            self.log_scenario_event("scenario_failed", {"error": str(exc)})
            self._log(f"Scenario failed: {exc}")
        finally:
            if traffic_thread is not None:
                traffic_thread.join(timeout=2.0)
            self.scenario_running = False
            self.finish_scenario_logging()
            self._ui(lambda: self.run_scenario_button.configure(state=tk.NORMAL))

    def begin_scenario_logging(self, scenario: dict, results_dir: Path):
        timestamp = datetime.now().strftime("%Y-%m-%d_%H-%M-%S")
        safe_name = "".join(char if char.isalnum() or char in "-_" else "_" for char in scenario["name"])
        run_dir = results_dir / f"{timestamp}_{safe_name}"
        suffix = 2
        while run_dir.exists():
            run_dir = results_dir / f"{timestamp}_{safe_name}_{suffix}"
            suffix += 1
        run_dir.mkdir(parents=True)

        self.current_run_dir = run_dir
        with self.scenario_data_lock:
            self.metrics_history = []
            self.scenario_events = []
        with (run_dir / "scenario.json").open("w", encoding="utf-8") as f:
            json.dump(scenario, f, indent=4)
            f.write("\n")
        self.log_scenario_event("scenario_started", {"scenario": scenario["name"]})

    def log_scenario_event(self, event_type: str, data: Optional[dict] = None):
        elapsed = 0.0
        if self.scenario_start_time is not None:
            elapsed = time.monotonic() - self.scenario_start_time
        event = {
            "timestamp": time.time(),
            "elapsed_seconds": elapsed,
            "event": event_type,
            **(data or {}),
        }
        with self.scenario_data_lock:
            self.scenario_events.append(event)
        self._log(f"SCENARIO [{elapsed:.2f}s] {event_type}: {data or {}}")

    def poll_statuses(self):
        runtimes = list(self.state.nodes.items())
        if not runtimes:
            return []
        statuses = []
        with ThreadPoolExecutor(max_workers=min(32, len(runtimes))) as pool:
            futures = [pool.submit(self._status_for_node, node_id, runtime) for node_id, runtime in runtimes]
            for future in as_completed(futures):
                _, status, _ = future.result()
                if status is not None:
                    statuses.append(status)
        return statuses

    def get_current_primary(self):
        statuses = self.poll_statuses()
        if not statuses:
            raise RuntimeError("No responsive nodes")
        primary_votes = {}
        for status in statuses:
            primary = int(status["current_primary"])
            primary_votes[primary] = primary_votes.get(primary, 0) + 1
        return max(primary_votes, key=primary_votes.get)

    def execute_fault(self, fault: dict):
        action = fault["action"]
        if action == "kill_primary":
            node_id = self.get_current_primary()
            self.kill_node(node_id)
            data = {"node_id": node_id}
        elif action == "pause_primary":
            node_id = self.get_current_primary()
            duration_ms = int(fault.get("duration_ms", 5000))
            self.pause_node(node_id, duration_ms)
            data = {"node_id": node_id, "duration_ms": duration_ms}
        elif action == "kill_node":
            node_id = int(fault["node_id"])
            self.kill_node(node_id)
            data = {"node_id": node_id}
        elif action == "pause_node":
            node_id = int(fault["node_id"])
            duration_ms = int(fault.get("duration_ms", 5000))
            self.pause_node(node_id, duration_ms)
            data = {"node_id": node_id, "duration_ms": duration_ms}
        else:
            raise ValueError(f"Unknown scenario fault: {action}")
        self.log_scenario_event(action, data)

    def finish_scenario_logging(self):
        if self.current_run_dir is None:
            return
        with self.scenario_data_lock:
            metrics = list(self.metrics_history)
            events = list(self.scenario_events)

        metric_fields = [
            "elapsed_seconds", "submit_tps", "commit_tps", "blocks_per_second",
            "commit_latency_ms", "responsive_nodes", "height_min", "height_max",
            "mempool_total", "view", "primary",
        ]
        with (self.current_run_dir / "metrics.csv").open("w", newline="", encoding="utf-8") as f:
            writer = csv.DictWriter(f, fieldnames=metric_fields)
            writer.writeheader()
            writer.writerows(metrics)

        with (self.current_run_dir / "events.log").open("w", encoding="utf-8") as f:
            for event in events:
                f.write(json.dumps(event, separators=(",", ":")) + "\n")

        summary = self.build_scenario_summary(metrics, events)
        with (self.current_run_dir / "summary.json").open("w", encoding="utf-8") as f:
            json.dump(summary, f, indent=4)
            f.write("\n")
        self._log(f"Scenario results saved to {self.current_run_dir}")

    def build_scenario_summary(self, metrics=None, events=None):
        metrics = list(self.metrics_history if metrics is None else metrics)
        events = list(self.scenario_events if events is None else events)
        scenario_name = self.current_scenario["name"] if self.current_scenario else None
        if not metrics:
            return {"scenario": scenario_name, "samples": 0, "events": len(events)}

        commit_tps_values = [float(sample["commit_tps"]) for sample in metrics]
        latency_values = [
            float(sample["commit_latency_ms"])
            for sample in metrics
            if sample["commit_latency_ms"] is not None
        ]
        return {
            "scenario": scenario_name,
            "samples": len(metrics),
            "average_commit_tps": sum(commit_tps_values) / len(commit_tps_values),
            "peak_commit_tps": max(commit_tps_values),
            "average_latency_ms": sum(latency_values) / len(latency_values) if latency_values else None,
            "final_height": metrics[-1]["height_max"],
            "events": len(events),
        }

    # ---------- faults / lifecycle ----------
    def pause_node(self, node_id: int, duration_ms: int):
        runtime = self.state.nodes.get(node_id)
        if runtime is None or runtime.process.poll() is not None or runtime.address is None:
            raise RuntimeError(f"Node {node_id} is not available")
        millis = max(1, int(duration_ms))
        self._send_json(runtime.address, {"Control": {"Pause": {"millis": millis}}})
        runtime.paused_until = time.monotonic() + millis / 1000.0
        self._log(f"Paused node {node_id} for {millis} ms")

    def kill_node(self, node_id: int):
        runtime = self.state.nodes.get(node_id)
        if runtime is None or runtime.process.poll() is not None:
            raise RuntimeError(f"Node {node_id} is not running")
        runtime.process.kill()
        self._log(f"Killed node {node_id} (pid {runtime.process.pid})")

    def pause_target(self, force_random=False):
        target = self._choose_target(force_random=force_random, for_fault=True)
        if target is None:
            return
        try:
            self.pause_node(target, self.pause_ms.get())
        except Exception as exc:
            self._log(f"Pause node {target} failed: {exc}")

    def kill_target(self, force_random=False):
        target = self._choose_target(force_random=force_random, for_fault=True)
        if target is None:
            return
        try:
            self.kill_node(target)
        except Exception as exc:
            self._log(f"Kill node {target} failed: {exc}")

    def stop_all(self):
        threading.Thread(target=self._stop_all_worker, daemon=True).start()

    def _stop_all_worker(self, force=False):
        self.state.running = False
        self.state.load_running = False
        self.stop_event.set()
        self.load_stop_event.set()
        self.load_wakeup_event.set()
        self.scenario_stop_event.set()
        if self.bootstrap_listener:
            try:
                self.bootstrap_listener.close()
            except OSError:
                pass
            self.bootstrap_listener = None
        for runtime in list(self.state.nodes.values()):
            if runtime.process.poll() is not None:
                continue
            if not force and runtime.address:
                try:
                    self._send_json(runtime.address, {"Control": "Shutdown"}, timeout=0.2)
                except Exception:
                    pass
        time.sleep(0.25)
        for runtime in list(self.state.nodes.values()):
            if runtime.process.poll() is None:
                try:
                    runtime.process.kill()
                except Exception:
                    pass
        self._log("Simulation stopped")

    def export_csv(self):
        if not self.state.metrics_history:
            messagebox.showinfo("No metrics", "No benchmark samples have been collected yet.")
            return
        path = filedialog.asksaveasfilename(
            title="Export benchmark CSV",
            defaultextension=".csv",
            filetypes=[("CSV", "*.csv"), ("All files", "*")],
        )
        if not path:
            return
        rows = list(self.state.metrics_history)
        with open(path, "w", newline="") as fh:
            writer = csv.DictWriter(fh, fieldnames=rows[0].keys())
            writer.writeheader()
            writer.writerows(rows)
        self._log(f"Exported metrics to {path}")

    # ---------- utilities ----------
    def _log(self, text: str):
        if not text:
            return
        self._ui(lambda t=text: self._append_log(t))

    def _append_log(self, text: str):
        timestamp = time.strftime("%H:%M:%S")
        self.log_text.insert(tk.END, f"[{timestamp}] {text}\n")
        self.log_text.see(tk.END)

    def _ui(self, fn):
        self.ui_queue.put(fn)

    def _drain_ui_queue(self):
        try:
            while True:
                fn = self.ui_queue.get_nowait()
                fn()
        except queue.Empty:
            pass
        self.root.after(100, self._drain_ui_queue)

    def _on_close(self):
        self._stop_all_worker(force=True)
        self.root.destroy()


if __name__ == "__main__":
    root = tk.Tk()
    app = ControlPlane(root)
    root.mainloop()
