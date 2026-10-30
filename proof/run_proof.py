#!/usr/bin/env python3
"""Forge 2.0 proof-suite harness (FRG-29, proposal §12).

Runs Forge 1.x and Forge 2.0 over each proof case (a defect diff
against the proof/scratch tree), records findings + decision logs +
wall-clock, runs the mechanical drop audit, and emits a report
skeleton whose verdict fields the human run fills in.

Subcommands (stdlib only):
  --list    list cases (no keys, no model calls)
  --check   validate fixtures only: tree files, diffs apply, case
            JSON schema-valid with expectations referencing real
            files/lines (no keys, no model calls)
  --run [case ...]
            execute the live evaluation (needs OPENROUTER_API_KEY;
            refuses cleanly without it). 1.x = PR-Agent plain-diff +
            thermos x2 + master LLM --dry-run. 2.0 = chomp review
            (--chomp must point at a release build).

Scoring is mechanical hints plus human verdicts: the harness checks
which expectations are textually satisfied and records them; the
caught/missed/false-positive calls live in report.json for the
operator. Cost note: neither chomp nor master emits token counts,
so the §10 comparison uses wall-clock here plus the runner's
OpenRouter dashboard for tokens.

Run:  python3 proof/run_proof.py --check
"""
import argparse
import glob
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
SCRATCH = os.path.join(HERE, "scratch")
CASES = os.path.join(HERE, "cases")
RESULTS = os.path.join(HERE, "results")
REPORT = os.path.join(HERE, "report.json")

REQUIRED_CASE_KEYS = {"name", "class", "title", "defect", "diff",
                      "expect_2x", "expect_1x", "fp_budget"}


def sh(cmd, env=None, timeout=1200):
    started = time.monotonic()
    proc = subprocess.run(cmd, capture_output=True, text=True,
                          env=env, timeout=timeout)
    return proc, time.monotonic() - started


def load_cases():
    cases = []
    for path in sorted(glob.glob(os.path.join(CASES, "*.json"))):
        with open(path, encoding="utf-8") as fh:
            cases.append((path, json.load(fh)))
    return cases


def check_case(path, case, scratch=None):
    """Validate one case fixture; return a list of error strings."""
    errors = []
    name = case.get("name", os.path.basename(path))
    missing = REQUIRED_CASE_KEYS - set(case)
    if missing:
        errors.append(f"{name}: missing keys {sorted(missing)}")
    diff_rel = case.get("diff", "")
    diff_path = os.path.join(CASES, diff_rel)
    if not diff_rel or not os.path.isfile(diff_path):
        errors.append(f"{name}: diff {diff_rel!r} not found")
        return errors
    for side, key in (("2x", "expect_2x"), ("1x", "expect_1x")):
        exp = case.get(key)
        if not isinstance(exp, dict) or "verdict" not in exp:
            errors.append(f"{name}: {key} needs a verdict")
    for field in ("must_cite", "must_mention"):
        for cited in case.get("expect_2x", {}).get(field, []) or []:
            if field == "must_cite" and scratch:
                full = os.path.join(scratch, cited)
                if not os.path.isfile(full):
                    errors.append(f"{name}: must_cite {cited!r} is not a scratch file")
    if scratch:
        proc, _ = sh(["git", "-C", scratch, "apply", "--check", diff_path])
        if proc.returncode != 0:
            errors.append(f"{name}: diff does not apply: {proc.stderr.strip()}")
    return errors


def init_scratch_clone(dest):
    """Fresh git checkout of the scratch tree on branch main."""
    if os.path.exists(dest):
        shutil.rmtree(dest)
    shutil.copytree(SCRATCH, dest, ignore=shutil.ignore_patterns("results"))
    sh(["git", "init", "-q", "-b", "main"], )[0]
    env = dict(os.environ, GIT_AUTHOR_NAME="proof", GIT_AUTHOR_EMAIL="p@p",
               GIT_COMMITTER_NAME="proof", GIT_COMMITTER_EMAIL="p@p")
    sh(["git", "-C", dest, "add", "-A"], env=env)
    proc, _ = sh(["git", "-C", dest, "commit", "-qm", "base"], env=env)
    if proc.returncode != 0:
        raise RuntimeError(f"scratch clone commit failed: {proc.stderr}")
    return dest


def ff(path):
    with open(path, encoding="utf-8") as fh:
        return fh.read()


def finding_text(finding):
    return " ".join(str(finding.get(k, "")) for k in
                    ("file", "title", "description", "evidence",
                     "recommended_fix", "fix_focus_areas", "citations"))


def hint_checks(expect, findings):
    """Mechanical hint evaluation: which expectations are textually satisfied."""
    hints = {}
    if expect.get("verdict") == "clean":
        hints["no_findings"] = (len(findings) == 0)
        return hints
    texts = [finding_text(f).lower() for f in findings]
    blobs = "\n".join(texts)
    for cited in expect.get("must_cite", []) or []:
        hints[f"cites:{cited}"] = any(
            f.get("file", "") == cited or cited in " ".join(
                f.get("citations", []) or []) for f in findings)
    for word in expect.get("must_mention", []) or []:
        hints[f"mentions:{word}"] = (word.lower() in blobs)
    hints["has_findings"] = len(findings) > 0
    return hints


def drop_audit(decisions):
    """Mechanical half of the §15 drop audit: every non-kept
    disposition must state a reason; kept/merged entries must name
    their final finding."""
    issues = []
    records = []
    entries = (decisions or {}).get("entries", []) or []
    for entry in entries:
        if not isinstance(entry, dict):
            continue
        if entry.get("kind") != "synthesis_disposition":
            continue
        disp = entry.get("disposition", "")
        rec = {"producer": entry.get("producer"),
               "cluster": entry.get("cluster"),
               "disposition": disp}
        records.append(rec)
        head, _, tail = disp.partition(":")
        if head in ("downgraded", "dropped"):
            if not (tail or "").strip():
                issues.append(f"producer {rec['producer']}: {head} without a reason")
            rec["reason"] = entry.get("reason", tail)
        elif head == "merged-into":
            if entry.get("final") is None:
                issues.append(f"producer {rec['producer']}: merged-into without a target")
        elif head == "kept":
            if entry.get("final") is None:
                issues.append(f"producer {rec['producer']}: kept without a target")
        else:
            issues.append(f"producer {rec['producer']}: unknown disposition {disp!r}")
    return records, issues


def run_1x(work, env, diff_rel="mr.diff"):
    """1.x generation: PR-Agent plain-diff + thermos x2 + master LLM dry-run."""
    forge = os.path.join(HERE, "..", ".forge")
    thermos = os.path.join(forge, "thermos.py")
    master = os.path.join(forge, "master.py")
    venv = os.environ.get("PROOF_VENV", "/tmp/proof-pr-agent-venv")
    outputs = {}
    if not os.path.isfile(os.path.join(venv, "bin", "python")):
        proc, dt = sh(["python3", "-m", "venv", venv])
        if proc.returncode != 0:
            return {"error": f"venv failed: {proc.stderr}"}, 0.0
        proc, _ = sh([os.path.join(venv, "bin", "pip"), "-q", "install",
                      "pr-agent[github]==0.49.0"])
        if proc.returncode != 0:
            return {"error": f"pr-agent install failed: {proc.stderr}"}, 0.0
    total = 0.0
    model = os.environ.get("FORGE_MODEL",
                           "openrouter/meta/muse-spark-1.3-contributor")
    penv = dict(env, CONFIG__MODEL=model,
                OPENROUTER__KEY=env["OPENROUTER_API_KEY"],
                CONFIG__CUSTOM_MODEL_MAX_TOKENS="1000000")
    proc, dt = sh([os.path.join(venv, "bin", "python"), "-m", "pr_agent.cli",
                   f"--diff-file={os.path.join(work, diff_rel)}",
                   f"--output={os.path.join(work, 'pr-agent.md')}", "review"],
                  env=penv, timeout=1500)
    total += dt
    outputs["pr_agent_rc"] = proc.returncode
    for lens in ("correctness", "structure"):
        out = os.path.join(work, f"thermos-{lens}.json")
        proc, dt = sh(["python3", thermos, "--lens", lens,
                       "--diff", os.path.join(work, diff_rel),
                       "--output", out], env=env, timeout=1500)
        total += dt
        outputs[f"thermos_{lens}_rc"] = proc.returncode
    ticket = os.path.join(work, "ticket.md")
    with open(ticket, "w", encoding="utf-8") as fh:
        fh.write("(no linked ticket)\n")
    proc, dt = sh(["python3", master, "--iid", "1", "--sha", "proof",
                   "--pr-agent", os.path.join(work, "pr-agent.md"),
                   "--thermos-a", os.path.join(work, "thermos-correctness.json"),
                   "--thermos-b", os.path.join(work, "thermos-structure.json"),
                   "--diff", os.path.join(work, diff_rel),
                   "--ticket", ticket, "--dry-run"], env=env, timeout=1500)
    total += dt
    outputs["master_rc"] = proc.returncode
    try:
        outputs["master_payload"] = json.loads(proc.stdout)
    except (ValueError, TypeError):
        outputs["master_error"] = proc.stderr[-2000:]
    return outputs, total


def run_2x(work, env, chomp, clone):
    """2.0 generation: chomp review over the scratch clone."""
    shutil.copy(os.path.join(clone, "mr.diff"), os.path.join(work, "mr.diff"))
    ticket = os.path.join(work, "ticket.md")
    with open(ticket, "w", encoding="utf-8") as fh:
        fh.write("(no linked ticket)\n")
    cmd = [chomp, "review", "--repo", clone,
           "--diff", os.path.join(work, "mr.diff"),
           "--base-ref", "main",
           "--pr-url", "https://github.com/example/proof/pull/1",
           "--ticket-file", ticket,
           "--out-findings", os.path.join(work, "chomp-findings.json"),
           "--out-decisions", os.path.join(work, "chomp-decisions.json"),
           "--out-pr-agent", os.path.join(work, "chomp-pr-agent.md")]
    proc, dt = sh(cmd, env=env, timeout=2700)
    outputs = {"chomp_rc": proc.returncode, "chomp_stderr_tail": proc.stderr[-2000:]}
    for name in ("chomp-findings.json", "chomp-decisions.json"):
        path = os.path.join(work, name)
        if os.path.isfile(path):
            with open(path, encoding="utf-8") as fh:
                outputs[name] = json.load(fh)
    return outputs, dt


def run_case(case, chomp, env):
    """Execute one case live; return the report skeleton entry."""
    name = case["name"]
    work = tempfile.mkdtemp(prefix=f"proof-{name}-")
    clone = os.path.join(work, "clone")
    init_scratch_clone(clone)
    proc, _ = sh(["git", "-C", clone, "apply",
                  os.path.join(CASES, case["diff"])])
    if proc.returncode != 0:
        raise RuntimeError(f"{name}: case diff failed to apply")
    proc, _ = sh(["git", "-C", clone, "diff", "--no-color"])
    with open(os.path.join(clone, "mr.diff"), "w", encoding="utf-8") as fh:
        fh.write(proc.stdout)
    out_1x, wall_1x = run_1x(work, env)
    out_2x, wall_2x = run_2x(work, env, chomp, clone)
    findings_1x = (out_1x.get("master_payload") or {}).get("findings", [])
    findings_2x = out_2x.get("chomp-findings.json", [])
    audit_records, audit_issues = drop_audit(
        out_2x.get("chomp-decisions.json", {}))
    dest = os.path.join(RESULTS, name)
    if os.path.exists(dest):
        shutil.rmtree(dest)
    os.makedirs(dest)
    for filename in ("pr-agent.md", "thermos-correctness.json",
                     "thermos-structure.json", "chomp-findings.json",
                     "chomp-decisions.json", "chomp-pr-agent.md",
                     "mr.diff", "ticket.md"):
        src = os.path.join(work, filename)
        if os.path.isfile(src):
            shutil.copy(src, dest)
    entry = {
        "name": name,
        "class": case["class"],
        "title": case.get("title", ""),
        "measurements": {
            "wall_1x_s": round(wall_1x, 1),
            "wall_2x_s": round(wall_2x, 1),
            "n_findings_1x": len(findings_1x),
            "n_findings_2x": len(findings_2x),
        },
        "hints_1x": hint_checks(case.get("expect_1x", {}), findings_1x),
        "hints_2x": hint_checks(case.get("expect_2x", {}), findings_2x),
        "drop_audit": {"records": audit_records, "issues": audit_issues},
        "verdict_1x": None,
        "verdict_2x": None,
        "fp_1x": None,
        "fp_2x": None,
    }
    return entry


def main(argv=None):
    parser = argparse.ArgumentParser(description="Forge 2.0 proof-suite harness")
    parser.add_argument("--list", action="store_true", help="list cases")
    parser.add_argument("--check", action="store_true",
                        help="validate fixtures only (no keys, no model calls)")
    parser.add_argument("--run", nargs="*", metavar="CASE",
                        help="execute live (needs OPENROUTER_API_KEY)")
    parser.add_argument("--chomp", default=None,
                        help="path to the chomp release binary (for --run)")
    args = parser.parse_args(argv)
    cases = load_cases()

    if args.list:
        for _, case in cases:
            print(f"{case.get('name')}  [class {case.get('class')}]  "
                  f"{case.get('title', '')}")
        return 0

    if args.check:
        with tempfile.TemporaryDirectory(prefix="proof-check-") as tmp:
            scratch = os.path.join(tmp, "scratch")
            shutil.copytree(SCRATCH, scratch)
            sh(["git", "init", "-q"], )[0]
            env = dict(os.environ, GIT_AUTHOR_NAME="p", GIT_AUTHOR_EMAIL="p@p",
                       GIT_COMMITTER_NAME="p", GIT_COMMITTER_EMAIL="p@p")
            sh(["git", "-C", scratch, "add", "-A"], env=env)
            sh(["git", "-C", scratch, "commit", "-qm", "base"], env=env)
            errors = []
            for path, case in cases:
                errors.extend(check_case(path, case, scratch))
        if errors:
            print("fixture errors:", file=sys.stderr)
            for error in errors:
                print(f"  - {error}", file=sys.stderr)
            return 1
        print(f"fixtures OK: {len(cases)} cases")
        return 0

    if args.run is not None:
        if "OPENROUTER_API_KEY" not in os.environ:
            print("proof run needs OPENROUTER_API_KEY in the environment",
                  file=sys.stderr)
            return 2
        if not args.chomp or not os.path.isfile(args.chomp):
            print("proof run needs --chomp pointing at a release build",
                  file=sys.stderr)
            return 2
        wanted = set(args.run or [])
        selected = [c for _, c in cases if not wanted or c.get("name") in wanted]
        if wanted:
            unknown = wanted - {c.get("name") for c in selected}
            if unknown:
                print(f"unknown cases: {sorted(unknown)}", file=sys.stderr)
                return 2
        os.makedirs(RESULTS, exist_ok=True)
        report = {"suite": "forge-2.0-proof", "cases": []}
        if os.path.isfile(REPORT):
            with open(REPORT, encoding="utf-8") as fh:
                try:
                    report = json.load(fh)
                except ValueError:
                    pass
        done = {c.get("name") for c in report.get("cases", [])}
        for case in selected:
            print(f"running {case['name']} ...", flush=True)
            entry = run_case(case, args.chomp, dict(os.environ))
            report["cases"] = [c for c in report.get("cases", [])
                               if c.get("name") != case["name"]] + [entry]
            with open(REPORT, "w", encoding="utf-8") as fh:
                json.dump(report, fh, indent=2)
            print(f"  1.x {entry['measurements']['wall_1x_s']}s / "
                  f"2.x {entry['measurements']['wall_2x_s']}s; "
                  f"findings {entry['measurements']['n_findings_1x']}/"
                  f"{entry['measurements']['n_findings_2x']}")
        print(f"report skeleton: {REPORT} (verdict fields await the human run)")
        return 0

    parser.print_help()
    return 2


if __name__ == "__main__":
    sys.exit(main())
