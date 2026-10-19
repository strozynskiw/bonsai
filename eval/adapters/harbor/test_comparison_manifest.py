"""No-network identity tests; synthetic digests are not benchmark evidence."""

import copy
import unittest

from eval.adapters.harbor.comparison_manifest import (
    DATASET, DSH_COMMIT, HARBOR_COMMIT, FrozenManifest, digest, freeze,
)


def manifest_fixture():
    artifact = {"revision": "fixture", "source_digest": digest("source"),
                "dirty": True, "artifact_digest": digest("binary"), "version": "fixture"}
    return {
        "schema_version": 1, "dataset": DATASET, "harbor_commit": HARBOR_COMMIT,
        "dataset_digest": digest("dataset"), "dependencies_digest": digest("dependencies"),
        "node": {"version": "fixture", "artifact_digest": digest("node")},
        "artifacts": {"bonsai": artifact, "dsh": {**artifact, "revision": DSH_COMMIT, "dirty": False}},
        "profiles": {name: {"overlay_digest": digest(name), "native_limits": "native defaults recorded"}
                     for name in ("bonsai", "dsh")},
        "model": {"id": "fixture-model", "endpoint": "https://provider.invalid/v1",
                  "reasoning": "high", "alias_limitation": "fixture only", "fallback": False},
        "resources": {"cpu": 2, "ram_mb": 4096, "disk_mb": 4096, "timeout_seconds": 30,
                      "network": "provider-only", "working_directory": "/app", "confinement": "container"},
        "tasks": [{"id": f"task-{i}", "content_digest": digest(i), "resource_digest": digest(i),
                   "image_digest": digest(i), "phase": "smoke" if i < 3 else "pilot"} for i in range(13)],
        "selection": {"seed": "fixture", "rule": "sha256-seed-task-v1",
                      "inventory_digest": digest("inventory"), "repetitions": 3},
        "exclusions": [{"id": "gpu", "reason": "requires GPU", "resource_digest": digest("gpu")}],
    }


class ManifestTests(unittest.TestCase):
    def test_canonical_digest_and_detached_identity(self):
        body = manifest_fixture()
        envelope = freeze(body)
        frozen = FrozenManifest.parse(envelope)
        body["model"]["id"] = "changed"
        self.assertEqual(frozen.body["model"]["id"], "fixture-model")
        view = frozen.body
        view["model"]["id"] = "mutated view"
        self.assertEqual(frozen.body["model"]["id"], "fixture-model")
        with self.assertRaisesRegex(ValueError, "digest mismatch"):
            FrozenManifest.parse(envelope)
        self.assertEqual(digest({"a": 1, "b": 2}), digest({"b": 2, "a": 1}))

    def test_reject_incomplete_or_misidentified_provenance(self):
        base = manifest_fixture()
        mutations = [
            lambda b: b.update(harbor_commit="wrong"),
            lambda b: b.update(dataset="terminal-bench@2.0"),
            lambda b: b["artifacts"]["bonsai"].update(artifact_digest=""),
            lambda b: b["artifacts"]["dsh"].update(dirty=True),
            lambda b: b["tasks"].append(b["tasks"][0]),
            lambda b: b["model"].update(fallback=True),
            lambda b: b["model"].update(endpoint="https://secret@provider.invalid"),
            lambda b: b["resources"].update(cpu=0),
            lambda b: b["selection"].update(repetitions=2),
        ]
        for mutate in mutations:
            body = copy.deepcopy(base)
            mutate(body)
            with self.subTest(body=body), self.assertRaises(ValueError):
                freeze(body)


if __name__ == "__main__":
    unittest.main()
