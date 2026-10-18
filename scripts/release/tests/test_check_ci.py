"""The release gate requires actual job success, not a green skipped workflow."""
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import re
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[3]
SPEC = importlib.util.spec_from_file_location("check_ci", ROOT / "scripts/release/check-ci.py")
CI = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CI)
SHA = "a" * 40


def run():
    return dict(id=123, head_sha=SHA, head_branch="main", event="push",
                actor={"login": "penso"}, triggering_actor={"login": "penso"},
                status="completed", conclusion="success")


def jobs():
    return [{"name": name, "status": "completed", "conclusion": "success"}
            for name in sorted(CI.REQUIRED_JOBS)]


class CheckCi(unittest.TestCase):
    def setUp(self):
        self.enterContext(contextlib.redirect_stdout(io.StringIO()))
        self.enterContext(contextlib.redirect_stderr(io.StringIO()))

    def check(self, runs, job_pages):
        with mock.patch.object(CI, "api", side_effect=[{"workflow_runs": runs}, job_pages]) as api:
            CI.require_ci(SHA)
            self.assertEqual(api.call_args, mock.call(
                "actions/runs/123/jobs?filter=latest&per_page=100", paginate=True))

    def test_full_push_and_manual_runs_pass_with_paginated_jobs(self):
        for event in ("push", "workflow_dispatch"):
            candidate = run()
            candidate["event"] = event
            with self.subTest(event=event):
                self.check([candidate], [{"jobs": jobs()[:3]}, {"jobs": jobs()[3:]}])

    def test_green_documentation_run_after_cancelled_code_run_is_rejected(self):
        cancelled = {**run(), "id": 122, "head_sha": "b" * 40, "conclusion": "cancelled"}
        skipped = [{**job, "conclusion": "skipped"} if "(" in job["name"] else job
                   for job in jobs()]
        with self.assertRaises(CI.CiError):
            self.check([run(), cancelled], [{"jobs": skipped}])

    def test_each_required_job_must_be_present_and_successful(self):
        for job in jobs():
            others = [other for other in jobs() if other["name"] != job["name"]]
            for conclusion in (None, "skipped", "failure", "cancelled"):
                with self.subTest(job=job["name"], conclusion=conclusion), self.assertRaises(CI.CiError):
                    self.check([run()], [{"jobs": others + [{**job, "conclusion": conclusion}]}])
            with self.subTest(missing=job["name"]), self.assertRaises(CI.CiError):
                self.check([run()], [{"jobs": others}])

    def test_other_commits_branches_events_and_actors_do_not_qualify(self):
        for changes in ({"head_sha": "b" * 40}, {"head_branch": "topic"},
                        {"event": "pull_request"}, {"actor": {"login": "other"}},
                        {"triggering_actor": {"login": "other"}},
                        {"conclusion": "failure"}, {"status": "in_progress"}):
            with self.subTest(changes=changes), mock.patch.object(
                    CI, "api", return_value={"workflow_runs": [{**run(), **changes}]}) as api:
                with self.assertRaises(CI.CiError):
                    CI.require_ci(SHA)
                api.assert_called_once()

    def test_wait_rechecks_jobs_instead_of_trusting_watch_exit_code(self):
        for conclusion in ("success", "skipped"):
            responses = [{"workflow_runs": [{**run(), "status": "in_progress", "conclusion": None}]},
                         {"workflow_runs": [run()]},
                         [{"jobs": [{**job, "conclusion": conclusion} for job in jobs()]}]]
            with self.subTest(conclusion=conclusion), mock.patch.object(CI, "api", side_effect=responses), \
                    mock.patch.object(CI, "gh") as gh:
                if conclusion == "success":
                    CI.require_ci(SHA, wait=True)
                else:
                    with self.assertRaises(CI.CiError):
                        CI.require_ci(SHA, wait=True)
                self.assertEqual(gh.call_args.args[:3], ("run", "watch", "123"))

    def test_api_errors_fail_closed(self):
        with mock.patch.object(CI, "api", side_effect=OSError("network unavailable")):
            with self.assertRaises(OSError):
                CI.require_ci(SHA)

    def test_successful_jobs_from_different_runs_are_not_combined(self):
        responses = [{"workflow_runs": [run(), {**run(), "id": 124}]},
                     [{"jobs": jobs()[:3]}], [{"jobs": jobs()[3:]}]]
        with mock.patch.object(CI, "api", side_effect=responses), self.assertRaises(CI.CiError):
            CI.require_ci(SHA)

    def test_no_runs_reports_how_to_start_full_ci(self):
        with mock.patch.object(CI, "api", return_value={"workflow_runs": []}), \
                self.assertRaisesRegex(CI.CiError, "gh workflow run ci.yml"):
            CI.require_ci(SHA, wait=True)

    def test_required_job_names_match_main_ci_matrix(self):
        workflow = (ROOT / ".github/workflows/ci.yml").read_text()
        sections = re.split(r"^  ([a-z-]+):\n", workflow.split("\njobs:\n", 1)[1], flags=re.M)
        definitions = dict(zip(sections[1::2], sections[2::2]))
        expected = set()
        for key in ("zizmor", "commits", "checks-passed", "checks", "windows", "windows-native", "build"):
            definition = definitions[key]
            name = re.search(r"^    name: (.+)$", definition, re.M)[1]
            if "${{ matrix.runner }}" in name:
                matrix = re.search(r"^        runner: (.+)$", definition, re.M)[1]
                # The last fromJSON alternative is main's matrix; PRs omit macOS 14.
                runners = (json.loads(re.findall(r"'(\[.*?\])'", matrix)[-1])
                           if "fromJSON" in matrix else matrix.strip("[]").split(", "))
                expected.update(name.replace("${{ matrix.runner }}", runner) for runner in runners)
            elif "${{ matrix.target }}" in name:
                expected.update(name.replace("${{ matrix.target }}", target)
                                for target in re.findall(r"^            target: (.+)$", definition, re.M))
            else:
                expected.add(name)
        self.assertEqual(CI.REQUIRED_JOBS, expected)
