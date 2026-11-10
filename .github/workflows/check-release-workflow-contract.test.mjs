import "./device-marker-auth-cases.mjs";
import "./release-branch-admission.test.mjs";
import "./source-provenance-link.test.mjs";
import "./check-release-identity.test.mjs";
import "./release-propagation.test.mjs";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { chmod, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import {
	checkReleaseWorkflow,
	parseWorkflow,
	validateReleaseWorkflow,
} from "./check-release-workflow-contract.mjs";

const releaseSha = "${{ needs.prepare.outputs.release_sha }}";
const completeWorkflow = `
permissions:
  contents: read
concurrency:
  group: \${{ github.workflow }}-\${{ startsWith(github.event.client_payload.version || inputs.version, 'v') && (github.event.client_payload.version || inputs.version) || format('v{0}', github.event.client_payload.version || inputs.version) }}
jobs:
  prepare:
    if: github.repository == 'dx-corp/code' && github.ref == 'refs/heads/main'
    runs-on: \${{ vars.PUBLIC_RELEASE_RUNNER || 'ubuntu-latest' }}
    permissions:
      contents: read
    outputs:
      package_name: \${{ steps.release.outputs.package_name }}
      release_sha: \${{ steps.release.outputs.release_sha }}
      release_tag: \${{ steps.release.outputs.release_tag }}
      release_version: \${{ steps.release.outputs.release_version }}
      release_channel: \${{ steps.release.outputs.release_channel }}
      npm_tag: \${{ steps.release.outputs.npm_tag }}
    steps:
      - uses: actions/checkout@sha
        with:
          fetch-depth: 0
      - id: release
        name: Resolve immutable release tag
        env:
          EVENT_NAME: \${{ github.event_name }}
          REQUESTED: \${{ github.event.client_payload.version || github.event.inputs.version || github.ref_name }}
          TRIGGER_SHA: \${{ github.sha }}
          GH_TOKEN: \${{ github.token }}
        run: |
          release_sha="$TRIGGER_SHA"
          if [[ "$EVENT_NAME" == "workflow_dispatch" || "$EVENT_NAME" == "repository_dispatch" ]]; then
          for attempt in 1 2 3; do
            if timeout 60s git \\
              -c http.lowSpeedLimit=1000 \\
              -c http.lowSpeedTime=30 \\
              fetch --force --no-tags origin "refs/tags/\${release_tag}:refs/tags/\${release_tag}"; then
              break
            fi
            if [[ "$attempt" -eq 3 ]]; then
              exit 1
            fi
            sleep 2
          done
          release_sha="$(git rev-list -n 1 "$release_tag")"
          elif [[ "$EVENT_NAME" != "push" ]]; then
            exit 1
          fi
          timeout 60s git fetch --no-tags origin main
          git merge-base --is-ancestor "$release_sha" FETCH_HEAD
          git checkout --detach "$release_sha"
          {
            echo "package_name=$package_name"
            echo "release_sha=$release_sha"
            echo "release_tag=$release_tag"
            echo "release_version=$release_version"
            echo "release_channel=$release_channel"
            echo "npm_tag=$npm_tag"
          } >> "$GITHUB_OUTPUT"
  identity-readiness:
    needs: prepare
    runs-on: \${{ vars.PUBLIC_RELEASE_RUNNER || 'ubuntu-latest' }}
    timeout-minutes: 3
    environment: npm-release
    permissions:
      contents: read
      id-token: write
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1
        with:
          ref: \${{ github.sha }}
          persist-credentials: false
          sparse-checkout: .github/workflows
      - name: Authenticate to the release-test Secret Manager lane
        uses: google-github-actions/auth@7c6bc770dae815cd3e89ee6cdf493a5fab2cc093
        with:
          workload_identity_provider: \${{ vars.MAESTRO_RELEASE_TEST_GCP_WORKLOAD_IDENTITY_PROVIDER }}
          service_account: \${{ vars.MAESTRO_RELEASE_TEST_GCP_SERVICE_ACCOUNT }}
          create_credentials_file: true
          export_environment_variables: true
      - name: Rotate and verify release-test Identity session
        env:
          MAESTRO_RELEASE_TEST_SESSION_SECRET: \${{ vars.MAESTRO_RELEASE_TEST_SESSION_SECRET }}
          MAESTRO_RELEASE_TEST_ORG_ID: \${{ vars.MAESTRO_RELEASE_TEST_ORG_ID }}
          MAESTRO_RELEASE_TEST_WORKSPACE_ID: \${{ vars.MAESTRO_RELEASE_TEST_WORKSPACE_ID }}
          MAESTRO_RELEASE_TEST_SUBJECT: \${{ vars.MAESTRO_RELEASE_TEST_SUBJECT }}
        run: node .github/workflows/refresh-release-identity.mjs
      - name: Verify release-test Identity session
        env:
          MAESTRO_EVALOPS_ORG_ID: \${{ vars.MAESTRO_RELEASE_TEST_ORG_ID }}
        run: node .github/workflows/check-release-identity.mjs

  binaries:
    needs: prepare
    permissions:
      contents: write
    steps:
      - uses: actions/checkout@sha
        with:
          ref: ${releaseSha}
      - name: Authenticate artifacts and release receipts
        env:
          RELEASE_VERSION: \${{ needs.prepare.outputs.release_version }}
        run: |
          node scripts/verify-staged-release.mjs release-binaries "$RELEASE_VERSION"
  publish:
    needs: [prepare, binaries, identity-readiness]
    runs-on: \${{ vars.PUBLIC_RELEASE_RUNNER || 'ubuntu-latest' }}
    environment:
      name: npm-release
    permissions:
      contents: read
      id-token: write
    steps:
      - uses: actions/checkout@sha
        with:
          ref: ${releaseSha}
      - uses: actions/setup-node@sha
        with:
          node-version: 24
          registry-url: https://registry.npmjs.org
      - name: Verify native package inputs
        run: |
          set -euo pipefail
          test "$(node -p "require('./package.json').version")" = "$RELEASE_VERSION"
          npm run check:rust-only-runtime
      - name: Publish to npm
        env:
          NODE_AUTH_TOKEN: \${{ secrets.NPM_TOKEN }}
          NPM_TAG: \${{ needs.prepare.outputs.npm_tag }}
          PACKAGE_NAME: \${{ needs.prepare.outputs.package_name }}
          ALIAS_PACKAGE_NAME: "@evalops/maestro"
          PACKED_INTEGRITY: \${{ steps.pack.outputs.integrity }}
          ALIAS_PACKED_INTEGRITY: \${{ steps.pack.outputs.alias_integrity }}
          RELEASE_VERSION: \${{ needs.prepare.outputs.release_version }}
          TARBALL: \${{ steps.pack.outputs.tarball }}
          ALIAS_TARBALL: \${{ steps.pack.outputs.alias_tarball }}
          NPM_CONFIG_FETCH_RETRIES: "1"
          NPM_CONFIG_FETCH_RETRY_MAXTIMEOUT: "2000"
          NPM_CONFIG_FETCH_RETRY_MINTIMEOUT: "1000"
          NPM_CONFIG_FETCH_TIMEOUT: "30000"
          NPM_CONFIG_REGISTRY: https://registry.npmjs.org
        run: |
          set -euo pipefail
          publish_with_oidc() {
            local tarball="$1"
            npx --yes npm@11.10.0 publish "\$tarball" --access public --tag "\$NPM_TAG" --registry "$NPM_CONFIG_REGISTRY"
          }
          publish_with_token() {
            local tarball="$1"
            if [[ -z "\${NODE_AUTH_TOKEN:-}" ]]; then
              return 1
            fi
            NPM_CONFIG_USERCONFIG="\$RUNNER_TEMP/npmrc" \\
              NODE_AUTH_TOKEN="\$NODE_AUTH_TOKEN" \\
              npx --yes npm@11.10.0 publish "\$tarball" --access public --tag "\$NPM_TAG" --registry "$NPM_CONFIG_REGISTRY"
          }
          verify_published_tarball() {
            local package_name="$1"
            local packed_integrity="$2"
            registry_integrity="\$(
              command npm view "\${package_name}@\${RELEASE_VERSION}" --registry "$NPM_CONFIG_REGISTRY" dist.integrity 2>/dev/null
            )" || return 1
            if [[ -z "\$registry_integrity" ]]; then
              return 1
            fi
            if [[ "\$registry_integrity" != "\$packed_integrity" ]]; then
              return 2
            fi
          }
          publish_or_verify() {
            local publisher="$1"
            local package_name="$2"
            local tarball="$3"
            local packed_integrity="$4"
            local publish_status=0
            local registry_status=0
            "\$publisher" "\$tarball" || publish_status=\$?
            if [[ "\$publish_status" -eq 0 ]]; then
              return 0
            fi
            verify_published_tarball "\$package_name" "\$packed_integrity" || registry_status=\$?
            if [[ "\$registry_status" -eq 0 ]]; then
              return 0
            fi
            if [[ "\$registry_status" -eq 2 ]]; then
              return 2
            fi
            return "\$publish_status"
          }
          publish_package() {
            local package_name="$1"
            local tarball="$2"
            local packed_integrity="$3"
            local registry_status=0
            verify_published_tarball "\$package_name" "\$packed_integrity" || registry_status=\$?
            if [[ "\$registry_status" -eq 0 ]]; then
              return 0
            fi
            if [[ "\$registry_status" -eq 2 ]]; then
              return 2
            fi
            if publish_or_verify publish_with_oidc "\$package_name" "\$tarball" "\$packed_integrity"; then
              return 0
            fi
            publish_or_verify publish_with_token "\$package_name" "\$tarball" "\$packed_integrity"
          }
          publish_package "\$PACKAGE_NAME" "\$TARBALL" "\$PACKED_INTEGRITY"
          wait_for_registry "\$PACKAGE_NAME" "\$PACKED_INTEGRITY"
          publish_package "\$ALIAS_PACKAGE_NAME" "\$ALIAS_TARBALL" "\$ALIAS_PACKED_INTEGRITY"
          wait_for_registry "\$ALIAS_PACKAGE_NAME" "\$ALIAS_PACKED_INTEGRITY"
      - name: Smoke packed package without JS runtime
        env:
          NPM_CONFIG_FETCH_RETRIES: "1"
          NPM_CONFIG_FETCH_RETRY_MAXTIMEOUT: "2000"
          NPM_CONFIG_FETCH_RETRY_MINTIMEOUT: "1000"
          NPM_CONFIG_FETCH_TIMEOUT: "30000"
          NPM_CONFIG_REGISTRY: https://registry.npmjs.org
        run: node scripts/smoke-packed-cli.js package.tgz
      - uses: actions/upload-artifact@sha
        with:
          name: npm-tarball-\${{ needs.prepare.outputs.release_tag }}
          overwrite: true
          path: package.tgz
  github-release:
    needs: [prepare, binaries, publish, post-publish-canary]
    runs-on: \${{ vars.PUBLIC_RELEASE_RUNNER || 'ubuntu-latest' }}
    permissions:
      contents: write
      id-token: write
    steps:
      - uses: actions/checkout@sha
        with:
          ref: ${releaseSha}
      - uses: actions/download-artifact@sha
        with:
          name: npm-tarball-\${{ needs.prepare.outputs.release_tag }}
          path: release-assets
      - uses: actions/download-artifact@sha
        with:
          pattern: maestro-*
          path: release-assets
          merge-multiple: true
      - name: Verify release tag has not moved
        env:
          EXPECTED_RELEASE_SHA: \${{ needs.prepare.outputs.release_sha }}
          RELEASE_TAG: \${{ needs.prepare.outputs.release_tag }}
        run: |
          for attempt in 1 2 3; do
            if timeout 60s git \\
              -c http.lowSpeedLimit=1000 \\
              -c http.lowSpeedTime=30 \\
              fetch --force --no-tags origin "refs/tags/\${RELEASE_TAG}:refs/tags/\${RELEASE_TAG}"; then
              break
            fi
            if [[ "$attempt" -eq 3 ]]; then
              exit 1
            fi
            sleep 2
          done
          current_release_sha="$(git rev-list -n 1 "$RELEASE_TAG")"
          if [[ "$current_release_sha" != "$EXPECTED_RELEASE_SHA" ]]; then
            exit 1
          fi
      - uses: softprops/action-gh-release@sha
        with:
          tag_name: \${{ needs.prepare.outputs.release_tag }}
          name: Maestro \${{ needs.prepare.outputs.release_version }}
          files: |
            release-assets/*.json
            release-assets/*.tgz
            release-assets/*.tar.gz
            release-assets/*.txt
            release-assets/*SUMS
            release-assets/*.bundle
            release-assets/maestro-linux-arm64
            release-assets/maestro-linux-x64
            release-assets/maestro-darwin-arm64
            release-assets/maestro-darwin-x64
  post-publish-canary:
    environment: npm-release
    needs:
      - prepare
      - publish
    runs-on: \${{ vars.PUBLIC_RELEASE_RUNNER || 'ubuntu-latest' }}
    permissions:
      contents: read
      id-token: write
    steps:
      - uses: actions/checkout@sha
        with:
          ref: ${releaseSha}
      - name: Check out protected workflow helpers
        uses: actions/checkout@sha
        with:
          ref: \${{ github.sha }}
          persist-credentials: false
          path: .release-workflow
          sparse-checkout: .github/workflows
      - uses: actions/setup-node@sha
        with:
          node-version: 24
          registry-url: https://registry.npmjs.org
      - name: Authenticate to the release-test Secret Manager lane
        uses: google-github-actions/auth@7c6bc770dae815cd3e89ee6cdf493a5fab2cc093
        with:
          workload_identity_provider: \${{ vars.MAESTRO_RELEASE_TEST_GCP_WORKLOAD_IDENTITY_PROVIDER }}
          service_account: \${{ vars.MAESTRO_RELEASE_TEST_GCP_SERVICE_ACCOUNT }}
          create_credentials_file: true
          export_environment_variables: true
      - name: Rotate release-test Identity session
        env:
          MAESTRO_RELEASE_TEST_SESSION_SECRET: \${{ vars.MAESTRO_RELEASE_TEST_SESSION_SECRET }}
          MAESTRO_RELEASE_TEST_ORG_ID: \${{ vars.MAESTRO_RELEASE_TEST_ORG_ID }}
          MAESTRO_RELEASE_TEST_WORKSPACE_ID: \${{ vars.MAESTRO_RELEASE_TEST_WORKSPACE_ID }}
          MAESTRO_RELEASE_TEST_SUBJECT: \${{ vars.MAESTRO_RELEASE_TEST_SUBJECT }}
        run: node .release-workflow/.github/workflows/refresh-release-identity.mjs
      - name: Verify published package from npm
        env:
          MAESTRO_EVALOPS_ORG_ID: \${{ vars.MAESTRO_RELEASE_TEST_ORG_ID }}
          NPM_CONFIG_FETCH_RETRIES: "1"
          NPM_CONFIG_FETCH_RETRY_MAXTIMEOUT: "2000"
          NPM_CONFIG_FETCH_RETRY_MINTIMEOUT: "1000"
          NPM_CONFIG_FETCH_TIMEOUT: "30000"
          NPM_CONFIG_REGISTRY: https://registry.npmjs.org
          PACKAGE_NAME: \${{ needs.prepare.outputs.package_name }}
          RELEASE_VERSION: \${{ needs.prepare.outputs.release_version }}
        run: |
          set -euo pipefail
          node scripts/smoke-registry-install.js \\
            --package "$PACKAGE_NAME" \\
            --version "$RELEASE_VERSION" \\
            --cli-command "$CLI_COMMAND"
      - name: Validate published replay evidence
        run: node scripts/verify-published-replay-evidence.js --evidence-dir published-replay-evidence
      - name: Upload published replay evidence
        uses: actions/upload-artifact@sha
        with:
          name: published-replay-evidence-\${{ needs.prepare.outputs.release_tag }}
          overwrite: true
          path: published-replay-evidence/*.json
`;

test("accepts mapping-form environment and the complete release contract", () => {
	assert.deepEqual(validateReleaseWorkflow(completeWorkflow), []);
});

test("rejects dormant npm publish helpers", () => {
	const dormant = completeWorkflow.replace(
		'publish_or_verify publish_with_token "$package_name" "$tarball" "$packed_integrity"',
		"          true\n",
	);
	assert.ok(
		validateReleaseWorkflow(dormant).some((failure) =>
			failure.includes("token-backed npm publication"),
		),
	);
});

test("rejects swallowed npm publish failures", () => {
	const swallowed = completeWorkflow.replaceAll(
		" --access public",
		" --access public || true",
	);
	assert.ok(
		validateReleaseWorkflow(swallowed).some((failure) =>
			failure.includes("exact unswallowed"),
		),
	);
});

test("comments cannot spoof environment, permissions, or dependencies", () => {
	const spoofed = completeWorkflow
		.replace(
			"  publish:\n    needs: [prepare, binaries, identity-readiness]\n    runs-on: ${{ vars.PUBLIC_RELEASE_RUNNER || 'ubuntu-latest' }}\n    environment:\n      name: npm-release\n    permissions:\n      contents: read\n      id-token: write\n",
			"  publish:\n    needs: [prepare, binaries, identity-readiness]\n    runs-on: ${{ vars.PUBLIC_RELEASE_RUNNER || 'ubuntu-latest' }}\n    environment:\n      name: npm-release\n    permissions:\n      # contents: read\n      id-token: write\n",
		)
		.replace("    environment:\n      name: npm-release\n", "    # environment: npm-release\n")
		.replace("      - publish\n", "      # - publish # needs: publish\n");
	const failures = validateReleaseWorkflow(spoofed);
	assert.ok(failures.some((failure) => failure.includes("npm-release environment")));
	assert.ok(failures.some((failure) => failure.includes("publish permissions")));
	assert.ok(failures.some((failure) => failure.includes("must need prepare and publish")));
});

test("rejects broad build permissions and non-serialized releases", () => {
	const broadened = completeWorkflow
		.replace("permissions:\n  contents: read\n", "permissions:\n  contents: write\n")
		.replace(
			"  group: ${{ github.workflow }}-${{ startsWith(github.event.client_payload.version || inputs.version, 'v') && (github.event.client_payload.version || inputs.version) || format('v{0}', github.event.client_payload.version || inputs.version) }}\n",
			"  group: ${{ github.workflow }}\n",
		)
		.replace(
			"  binaries:\n    needs: prepare\n    permissions:\n      contents: write\n",
			"  binaries:\n    needs: prepare\n    permissions:\n      contents: write\n      id-token: write\n",
		);
	const failures = validateReleaseWorkflow(broadened);
	assert.ok(failures.some((failure) => failure.includes("default permissions")));
	assert.ok(failures.some((failure) => failure.includes("serialize")));
	assert.ok(failures.some((failure) => failure.includes("binaries permissions")));
});

test("rejects extra workflow or job write permissions", () => {
	const broadened = completeWorkflow
		.replace("permissions:\n  contents: read\n", "permissions:\n  contents: read\n  packages: write\n")
		.replace(
			"  binaries:\n    needs: prepare\n    permissions:\n      contents: write\n",
			"  binaries:\n    needs: prepare\n    permissions:\n      contents: write\n      actions: write\n",
		);
	const failures = validateReleaseWorkflow(broadened);
	assert.ok(failures.some((failure) => failure.includes("default permissions")));
	assert.ok(failures.some((failure) => failure.includes("binaries permissions")));
});

test("rejects publish checkout without contents read", () => {
	const unreadablePublish = completeWorkflow.replace(
		"  publish:\n    needs: [prepare, binaries, identity-readiness]\n    runs-on: ${{ vars.PUBLIC_RELEASE_RUNNER || 'ubuntu-latest' }}\n    environment:\n      name: npm-release\n    permissions:\n      contents: read\n      id-token: write\n",
		"  publish:\n    needs: [prepare, binaries, identity-readiness]\n    runs-on: ${{ vars.PUBLIC_RELEASE_RUNNER || 'ubuntu-latest' }}\n    environment:\n      name: npm-release\n    permissions:\n      id-token: write\n",
	);
	assert.ok(
		validateReleaseWorkflow(unreadablePublish).some((failure) =>
			failure.includes("publish permissions"),
		),
	);
});

test("rejects workflow_dispatch work from an unbound ref", () => {
	const unbound = completeWorkflow.replaceAll(
		`          ref: ${releaseSha}`,
		"          ref: main",
	);
	assert.ok(
		validateReleaseWorkflow(unbound).some((failure) =>
			failure.includes("immutable release SHA"),
		),
	);
});

test("rejects shallow history that cannot prove protected main ancestry", () => {
	const fullHistory = completeWorkflow.replace(
		"          fetch-depth: 0\n",
		"          fetch-depth: 1\n",
	);
	assert.ok(
		validateReleaseWorkflow(fullHistory).some((failure) =>
			failure.includes("prepare checkout must include history"),
		),
	);
});

test("rejects tag-push source replacement with a freshly resolved tag", () => {
	const movedTag = completeWorkflow
		.replace('          release_sha="$TRIGGER_SHA"\n', "")
		.replace(
			'          if [[ "$EVENT_NAME" == "workflow_dispatch" || "$EVENT_NAME" == "repository_dispatch" ]]; then\n',
			"",
		)
		.replace(
			'          elif [[ "$EVENT_NAME" != "push" ]]; then\n            exit 1\n          fi\n',
			"",
		);
	assert.ok(
		validateReleaseWorkflow(movedTag).some((failure) =>
			failure.includes("tag pushes must preserve the triggering SHA"),
		),
	);
});

test("rejects an unbound triggering event or SHA", () => {
	const reboundTrigger = completeWorkflow.replace(
		"          TRIGGER_SHA: ${{ github.sha }}",
		"          TRIGGER_SHA: ${{ github.ref_name }}",
	);
	assert.ok(
		validateReleaseWorkflow(reboundTrigger).some((failure) =>
			failure.includes("bind the triggering event and SHA"),
		),
	);
});

test("rejects a later checkout that replaces the immutable release source", () => {
	const secondCheckout = completeWorkflow.replace(
		`      - name: Verify native package inputs
`,
		`      - uses: actions/checkout@sha
        with:
          ref: main
      - name: Verify native package inputs
`,
	);
	assert.ok(
		validateReleaseWorkflow(secondCheckout).some((failure) =>
			failure.includes("publish checkout must bind"),
		),
	);
});

test("rejects a rebound prepare release SHA output", () => {
	const rebound = completeWorkflow.replace(
		"      release_sha: ${{ steps.release.outputs.release_sha }}",
		"      release_sha: ${{ github.sha }}",
	);
	assert.ok(
		validateReleaseWorkflow(rebound).some((failure) =>
			failure.includes("outputs must bind exactly"),
		),
	);
});

test("rejects rebound or replaced immutable release metadata", () => {
	const reboundTag = completeWorkflow.replace(
		"      release_tag: ${{ steps.release.outputs.release_tag }}",
		"      release_tag: ${{ github.ref_name }}",
	);
	assert.ok(
		validateReleaseWorkflow(reboundTag).some((failure) =>
			failure.includes("outputs must bind exactly"),
		),
	);

	const replacedTag = completeWorkflow.replace(
		'            echo "release_tag=$release_tag"',
		'            echo "release_tag=$GITHUB_REF_NAME"',
	);
	assert.ok(
		validateReleaseWorkflow(replacedTag).some((failure) =>
			failure.includes("emit release_tag exactly once"),
		),
	);

	const replacedPackage = completeWorkflow.replace(
		'            echo "package_name=$package_name"',
		'            echo "package_name=@attacker/package"',
	);
	assert.ok(
		validateReleaseWorkflow(replacedPackage).some((failure) =>
			failure.includes("emit package_name exactly once"),
		),
	);
});

test("rejects a fake release output step split from the immutable resolver", () => {
	const splitIdentity = completeWorkflow
		.replace(
			"      - id: release\n",
			`      - id: release
        run: echo "release_sha=$GITHUB_SHA" >> "$GITHUB_OUTPUT"
      - id: immutable_release
`,
		);
	assert.ok(
		validateReleaseWorkflow(splitIdentity).some((failure) =>
			failure.includes("step id must be release"),
		),
	);
});

test("rejects resolver output replacement and conditional jobs", () => {
	const replacedOutput = completeWorkflow.replace(
		'          echo "release_sha=$release_sha"',
		'          echo "release_sha=$GITHUB_SHA"',
	);
	assert.ok(
		validateReleaseWorkflow(replacedOutput).some((failure) =>
			failure.includes("emit release_sha exactly once"),
		),
	);

	const skippedPublish = completeWorkflow.replace(
		"  publish:\n",
		"  publish:\n    if: ${{ false }}\n",
	);
	assert.ok(
		validateReleaseWorkflow(skippedPublish).some((failure) =>
			failure.includes("publish job must not be conditional"),
		),
	);
});

test("rejects disabled or shell-dormant npm publication", () => {
	const disabledStep = completeWorkflow.replace(
		"      - name: Publish to npm\n",
		"      - name: Publish to npm\n        if: ${{ false }}\n",
	);
	assert.ok(
		validateReleaseWorkflow(disabledStep).some((failure) =>
			failure.includes("publication must not be conditional or ignored"),
		),
	);

	const dormantBranch = completeWorkflow.replace(
		'publish_or_verify publish_with_token "$package_name" "$tarball" "$packed_integrity"',
		'if false; then\n            publish_or_verify publish_with_token "$package_name" "$tarball" "$packed_integrity"\n          fi',
	);
	assert.ok(
		validateReleaseWorkflow(dormantBranch).some((failure) =>
			failure.includes("token-backed npm publication"),
		),
	);
});

test("rejects continue-on-error on required release steps", () => {
	for (const stepName of [
		"Resolve immutable release tag",
		"Publish to npm",
		"Verify published package from npm",
	]) {
		const property = `        name: ${stepName}
`;
		const stepStart = `      - name: ${stepName}
`;
		const ignored = completeWorkflow.includes(property)
			? completeWorkflow.replace(
					property,
					`${property}        continue-on-error: true
`,
				)
			: completeWorkflow.replace(
					stepStart,
					`${stepStart}        continue-on-error: true
`,
				);
		assert.notDeepEqual(
			validateReleaseWorkflow(ignored),
			[],
			`${stepName} must not allow ignored failures`,
		);
	}
});

test("rejects a replay validation step that does not execute the verifier", () => {
	const bypassed = completeWorkflow.replace(
		"        run: node scripts/verify-published-replay-evidence.js --evidence-dir published-replay-evidence\n",
		"        run: true\n",
	);
	assert.ok(
		validateReleaseWorkflow(bypassed).some((failure) =>
			failure.includes("exact evidence verifier"),
		),
	);
});

test("rejects early success before replay or registry validation", () => {
	const replayExit = completeWorkflow.replace(
		"        run: node scripts/verify-published-replay-evidence.js --evidence-dir published-replay-evidence\n",
		"        run: |\n          exit 0\n          node scripts/verify-published-replay-evidence.js --evidence-dir published-replay-evidence\n",
	);
	assert.ok(
		validateReleaseWorkflow(replayExit).some((failure) =>
			failure.includes("exact evidence verifier"),
		),
	);

	const canaryExit = completeWorkflow.replace(
		"          set -euo pipefail\n          node scripts/smoke-registry-install.js",
		"          exit 0\n          set -euo pipefail\n          node scripts/smoke-registry-install.js",
	);
	assert.ok(
		validateReleaseWorkflow(canaryExit).some((failure) =>
			failure.includes("exact registry smoke"),
		),
	);
});

test("rejects alternate publish failure swallowing", () => {
	const swallowed = completeWorkflow.replaceAll(
		" --access public",
		" --access public || :",
	);
	assert.ok(
		validateReleaseWorkflow(swallowed).some((failure) =>
			failure.includes("exact unswallowed"),
		),
	);
});

test("rejects unauthorized early success or a shell function that shadows npm", () => {
	const earlyExit = completeWorkflow.replace(
		'publish_package "$PACKAGE_NAME" "$TARBALL" "$PACKED_INTEGRITY"',
		'exit 0\n          publish_package "$PACKAGE_NAME" "$TARBALL" "$PACKED_INTEGRITY"',
	);
	assert.ok(
		validateReleaseWorkflow(earlyExit).some((failure) =>
			failure.includes("must not be bypassed"),
		),
	);

	const shadowed = completeWorkflow.replace(
		"          publish_with_token() {\n",
		"          npm() { return 0; }\n          publish_with_token() {\n",
	);
	assert.ok(
		validateReleaseWorkflow(shadowed).some((failure) =>
			failure.includes("must not be bypassed"),
		),
	);

	const swallowedTokenFailure = completeWorkflow.replace(
		'npx --yes npm@11.10.0 publish "$tarball" --access public --tag "$NPM_TAG" --registry "$NPM_CONFIG_REGISTRY"',
		'npx --yes npm@11.10.0 publish "$tarball" --access public --tag "$NPM_TAG" --registry "$NPM_CONFIG_REGISTRY"\n            return 0',
	);
	assert.ok(
		validateReleaseWorkflow(swallowedTokenFailure).some((failure) =>
			failure.includes("reconcile the exact registry tarball"),
		),
	);
});

test("requires exact registry reconciliation after errors and on reruns", () => {
	const noPreflight = completeWorkflow.replace(
		'verify_published_tarball "$package_name" "$packed_integrity" || registry_status=$?',
		"false || registry_status=$?",
	);
	assert.ok(
		validateReleaseWorkflow(noPreflight).some((failure) =>
			failure.includes("reconcile the exact registry tarball"),
		),
	);

	const noErrorRecovery = completeWorkflow.replace(
		'verify_published_tarball "$package_name" "$packed_integrity" || registry_status=$?',
		"false || registry_status=$?",
	);
	assert.ok(
		validateReleaseWorkflow(noErrorRecovery).some((failure) =>
			failure.includes("reconcile the exact registry tarball"),
		),
	);

	const noIntegrityBinding = completeWorkflow.replace(
		"          PACKED_INTEGRITY: \${{ steps.pack.outputs.integrity }}\n",
		"",
	);
	assert.ok(
		validateReleaseWorkflow(noIntegrityBinding).some((failure) =>
			failure.includes("exact package identity"),
		),
	);
});

test("rejects npm publication or reconciliation without the public registry", () => {
	const inheritedRegistry = completeWorkflow.replaceAll(
		' --registry "$NPM_CONFIG_REGISTRY"',
		"",
	);
	assert.ok(
		validateReleaseWorkflow(inheritedRegistry).some((failure) =>
			failure.includes("exact unswallowed npm publish commands"),
		),
	);

	const unboundRegistry = completeWorkflow.replaceAll(
		"          NPM_CONFIG_REGISTRY: https://registry.npmjs.org\n",
		"",
	);
	assert.ok(
		validateReleaseWorkflow(unboundRegistry).some((failure) =>
			failure.includes("bounded npm network configuration"),
		),
	);

	const setupRegistryMissing = completeWorkflow.replaceAll(
		"          registry-url: https://registry.npmjs.org\n",
		"",
	);
	assert.ok(
		validateReleaseWorkflow(setupRegistryMissing).some((failure) =>
			failure.includes("pin setup-node to the public npm registry"),
		),
	);
});

test("rejects removing the token-backed publish fallback", () => {
	const trusted = completeWorkflow.replace(
		"          publish_with_token() {\n",
		"          publish_with_trusted_publisher() {\n",
	);
	assert.ok(
		validateReleaseWorkflow(trusted).some((failure) =>
			failure.includes("token-backed npm publication"),
		),
	);
});

test("rejects a GitHub release job without npm publication dependency", () => {
	const outOfOrder = completeWorkflow.replace(
		"    needs: [prepare, binaries, publish, post-publish-canary]\n",
		"    needs: [prepare, binaries, identity-readiness]\n",
	);
	assert.ok(
		validateReleaseWorkflow(outOfOrder).some((failure) =>
			failure.includes("github-release must need"),
		),
	);
});

test("rejects an unbounded immutable tag fetch", () => {
	const unbounded = completeWorkflow.replace(
		`          for attempt in 1 2 3; do
            if timeout 60s git \\
              -c http.lowSpeedLimit=1000 \\
              -c http.lowSpeedTime=30 \\
              fetch --force --no-tags origin "refs/tags/\${release_tag}:refs/tags/\${release_tag}"; then
              break
            fi
            if [[ "$attempt" -eq 3 ]]; then
              exit 1
            fi
            sleep 2
          done
`,
		`          git fetch --force --no-tags origin "refs/tags/\${release_tag}:refs/tags/\${release_tag}"
`,
	);
	assert.ok(
		validateReleaseWorkflow(unbounded).some((failure) =>
			failure.includes("bounded retries"),
		),
	);
});

test("rejects release control jobs outside the public release runner", () => {
	const wrongRunner = completeWorkflow.replace(
		"    runs-on: ${{ vars.PUBLIC_RELEASE_RUNNER || 'ubuntu-latest' }}",
		"    runs-on: ubuntu-latest",
	);
	assert.ok(
		validateReleaseWorkflow(wrongRunner).some((failure) =>
			failure.includes("must run on PUBLIC_RELEASE_RUNNER"),
		),
	);
});

test("rejects unbounded npm registry calls in publish and canary", () => {
	const unboundedPublish = completeWorkflow.replace(
		'          NPM_CONFIG_FETCH_TIMEOUT: "30000"\n          NPM_CONFIG_REGISTRY: https://registry.npmjs.org\n',
		'          NPM_CONFIG_REGISTRY: https://registry.npmjs.org\n',
	);
	assert.ok(
		validateReleaseWorkflow(unboundedPublish).some((failure) =>
			failure.includes("publish must set bounded npm network configuration"),
		),
	);

	const lastTimeout = completeWorkflow.lastIndexOf(
		'          NPM_CONFIG_FETCH_TIMEOUT: "30000"\n',
	);
	const unboundedCanary =
		completeWorkflow.slice(0, lastTimeout) +
		completeWorkflow
			.slice(lastTimeout)
			.replace('          NPM_CONFIG_FETCH_TIMEOUT: "30000"\n', "");
	assert.ok(
		validateReleaseWorkflow(unboundedCanary).some((failure) =>
			failure.includes(
				"post-publish canary must set bounded npm network configuration",
			),
		),
	);

	const smokeName = "      - name: Smoke packed package without JS runtime\n";
	const smokeStart = completeWorkflow.indexOf(smokeName);
	const unboundedSmoke =
		completeWorkflow.slice(0, smokeStart) +
		completeWorkflow
			.slice(smokeStart)
			.replace('          NPM_CONFIG_FETCH_TIMEOUT: "30000"\n', "");
	assert.ok(
		validateReleaseWorkflow(unboundedSmoke).some((failure) =>
			failure.includes(
				"packed-package smoke must set bounded npm network configuration",
			),
		),
	);
});

test("rejects a non-retryable or incomplete GitHub release job", () => {
	const inPublish = completeWorkflow
		.replace(
			`      - uses: softprops/action-gh-release@sha
        with:
          tag_name: \${{ needs.prepare.outputs.release_tag }}
          name: Maestro \${{ needs.prepare.outputs.release_version }}
          files: |
            release-assets/*.json
            release-assets/*.tgz
            release-assets/*.tar.gz
            release-assets/*.txt
            release-assets/*SUMS
            release-assets/*.bundle
            release-assets/maestro-linux-arm64
            release-assets/maestro-linux-x64
            release-assets/maestro-darwin-arm64
            release-assets/maestro-darwin-x64
`,
			"",
		)
		.replace(
			"      - name: Publish to npm\n",
			`      - uses: softprops/action-gh-release@sha
        with:
          tag_name: \${{ needs.prepare.outputs.release_tag }}
          name: Maestro \${{ needs.prepare.outputs.release_version }}
          files: |
            release-assets/*.json
            release-assets/*.tgz
            release-assets/*.tar.gz
            release-assets/*.txt
            release-assets/*SUMS
            release-assets/*.bundle
            release-assets/maestro-linux-arm64
            release-assets/maestro-linux-x64
            release-assets/maestro-darwin-arm64
            release-assets/maestro-darwin-x64
      - name: Publish to npm
`,
		);
	assert.ok(
		validateReleaseWorkflow(inPublish).some((failure) =>
			failure.includes("retryable github-release job"),
		),
	);

	const missingArtifact = completeWorkflow.replace(
		"          name: npm-tarball-${{ needs.prepare.outputs.release_tag }}",
		"          name: wrong-npm-artifact",
	);
	assert.ok(
		validateReleaseWorkflow(missingArtifact).some((failure) =>
			failure.includes("retryable artifact") ||
			failure.includes("exact immutable release artifacts"),
		),
	);

	const nonReplaceableArtifact = completeWorkflow.replace(
		"          overwrite: true\n          path: package.tgz",
		"          path: package.tgz",
	);
	assert.ok(
		validateReleaseWorkflow(nonReplaceableArtifact).some((failure) =>
			failure.includes("persist retryable artifact"),
		),
	);
});

test("rejects a GitHub release detached from immutable prepare metadata", () => {
	const wrongTag = completeWorkflow.replace(
		"          tag_name: ${{ needs.prepare.outputs.release_tag }}",
		"          tag_name: main",
	);
	assert.ok(
		validateReleaseWorkflow(wrongTag).some((failure) =>
			failure.includes("metadata and files must bind"),
		),
	);
});

test("rejects GitHub release creation after a moved tag", () => {
	const noTagCheck = completeWorkflow.replace(
		'          if [[ "$current_release_sha" != "$EXPECTED_RELEASE_SHA" ]]; then\n',
		'          if [[ "$current_release_sha" == "$EXPECTED_RELEASE_SHA" ]]; then\n',
	);
	assert.ok(
		validateReleaseWorkflow(noTagCheck).some((failure) =>
			failure.includes("fail closed if the release tag moved"),
		),
	);

	const unboundTagCheck = completeWorkflow.replace(
		"          EXPECTED_RELEASE_SHA: ${{ needs.prepare.outputs.release_sha }}",
		"          EXPECTED_RELEASE_SHA: ${{ github.sha }}",
	);
	assert.ok(
		validateReleaseWorkflow(unboundTagCheck).some((failure) =>
			failure.includes("verify the current tag immediately"),
		),
	);
});

test("rejects GitHub release retargeting of a detached commit", () => {
	const retargeted = completeWorkflow.replace(
		`      - uses: softprops/action-gh-release@sha
        with:
          tag_name: \${{ needs.prepare.outputs.release_tag }}
          name: Maestro \${{ needs.prepare.outputs.release_version }}
          files: |
            release-assets/*.json
            release-assets/*.tgz
            release-assets/*.tar.gz
            release-assets/*.txt
            release-assets/*SUMS
            release-assets/*.bundle
            release-assets/maestro-linux-arm64
            release-assets/maestro-linux-x64
            release-assets/maestro-darwin-arm64
            release-assets/maestro-darwin-x64
`,
		`      - uses: softprops/action-gh-release@sha
        with:
          tag_name: \${{ needs.prepare.outputs.release_tag }}
          target_commitish: \${{ needs.prepare.outputs.release_sha }}
          name: Maestro \${{ needs.prepare.outputs.release_version }}
          files: |
            release-assets/*.json
            release-assets/*.tgz
            release-assets/*.tar.gz
            release-assets/*.txt
            release-assets/*SUMS
            release-assets/*.bundle
            release-assets/maestro-linux-arm64
            release-assets/maestro-linux-x64
            release-assets/maestro-darwin-arm64
            release-assets/maestro-darwin-x64
`,
	);
	assert.ok(
		validateReleaseWorkflow(retargeted).some((failure) =>
			failure.includes("must not retarget a detached commit"),
		),
	);
});

test("rejects an additional GitHub release action with a different tag", () => {
	const duplicate = completeWorkflow.replace(
		"  post-publish-canary:\n",
		`      - uses: softprops/action-gh-release@sha
        with:
          tag_name: v999.0.0
          name: Wrong release
  post-publish-canary:
`,
	);
	assert.ok(
		validateReleaseWorkflow(duplicate).some((failure) =>
			failure.includes("exactly one GitHub release action"),
		),
	);
});

test("rejects non-replaceable post-publish replay evidence", () => {
	const nonReplaceable = completeWorkflow.replace(
		"          name: published-replay-evidence-${{ needs.prepare.outputs.release_tag }}\n          overwrite: true\n",
		"          name: published-replay-evidence-${{ needs.prepare.outputs.release_tag }}\n",
	);
	assert.ok(
		validateReleaseWorkflow(nonReplaceable).some((failure) =>
			failure.includes("replace exact-tag replay evidence"),
		),
	);
});

test("current release workflow satisfies the parsed contract", async () => {
	assert.deepEqual(await checkReleaseWorkflow(), []);
});

test("required CI tooling lane runs the release workflow contracts", async () => {
	const ciTooling = await readFile(
		new URL("../../scripts/run-ci-tooling.sh", import.meta.url),
		"utf8",
	);
	assert.match(
		ciTooling,
		/node --test \.github\/workflows\/check-release-workflow-contract\.test\.mjs/u,
	);
	assert.match(
		ciTooling,
		/node \.github\/workflows\/check-release-workflow-contract\.mjs/u,
	);
});

for (const [name, step] of [
	["browser input requirement", "      - run: test -f packages/web/dist/index.html"],
	["browser archive packaging", "      - run: tar -czf maestro-web-dist.tar.gz -C packages/web/dist ."],
	["browser archive signing", "      - run: sha256sum maestro-web-dist.tar.gz > SHA256SUMS"],
	["browser artifact upload", `      - uses: actions/upload-artifact@sha
        with:
          name: release-web-dist-${releaseSha}
          path: maestro-web-dist.tar.gz`],
	["browser artifact download", `      - uses: actions/download-artifact@sha
        with:
          name: release-web-dist-${releaseSha}
          path: release-assets`],
]) {
	test(`rejects obsolete ${name}`, () => {
		assert.deepEqual(validateReleaseWorkflow(completeWorkflow), []);
		const workflow = completeWorkflow.replace(
			"      - name: Verify release tag has not moved",
			`${step}\n      - name: Verify release tag has not moved`,
		);
		assert.ok(validateReleaseWorkflow(workflow).some((failure) =>
			failure.includes("obsolete browser assets"),
		));
	});
}

for (const line of [
	`          test "$(node -p "require('./package.json').version")" = "$RELEASE_VERSION"`,
	"          npm run check:rust-only-runtime",
]) {
	test(`rejects missing native input check: ${line.trim()}`, () => {
		assert.ok(validateReleaseWorkflow(completeWorkflow.replace(line, "")).some((failure) =>
			failure.includes("exact native package input checks"),
		));
	});
}

for (const line of ['          git merge-base --is-ancestor "$release_sha" FETCH_HEAD', '          timeout 60s git fetch --no-tags origin main']) {
 test(`rejects missing ancestry guard: ${line.trim()}`, () => {
  assert.ok(validateReleaseWorkflow(completeWorkflow.replace(line, "")).some(f => f.includes("ancestry verification")));
 });
}

async function readGhcrPublisher() {
	const source = await readFile(new URL("./ghcr-publish.yml", import.meta.url), "utf8");
	const job = parseWorkflow(source).jobs["publish-image"];
	assert.ok(job, "GHCR publisher job exists");
	const image = source.match(/^  IMAGE_NAME:\s+(\S+)$/mu)?.[1];
	assert.ok(image, "publisher has one canonical image reference");
	return { source, job, image };
}

async function runGhcrShellStep(t, script, image, overrides = {}) {
	const directory = await mkdtemp(join(tmpdir(), "code publisher contract-"));
	t.after(() => rm(directory, { recursive: true, force: true }));
	const bin = join(directory, "bin");
	await mkdir(bin);
	await writeFile(join(bin, "docker"), `#!/bin/sh
printf '%s\\n' "$@" > "$DOCKER_CALLS"
if [ "$DOCKER_FAIL_ONCE" = 1 ] && [ ! -e "$DOCKER_ATTEMPTED" ]; then
  touch "$DOCKER_ATTEMPTED"
  exit 1
fi
`);
	await writeFile(join(bin, "gh"), '#!/bin/sh\necho "unexpected deployment dispatch" >&2\nexit 91\n');
	await writeFile(join(bin, "git"), `#!/bin/sh
printf '%s\\n' "$@" > "$GIT_CALLS"
if [ "$LIVE_MAIN_STATUS" != 0 ]; then
  echo "fatal: remote HTTP 503" >&2
  exit "$LIVE_MAIN_STATUS"
fi
if [ -e "$GIT_ATTEMPTED" ] && [ -n "$NEXT_MAIN_RESPONSE" ]; then
  printf '%s\\n' "$NEXT_MAIN_RESPONSE"
else
  touch "$GIT_ATTEMPTED"
  printf '%s\\n' "$LIVE_MAIN_RESPONSE"
fi
`);
	await writeFile(join(bin, "sleep"), "#!/bin/sh\nexit 0\n");
	await chmod(join(bin, "sleep"), 0o755);
	await chmod(join(bin, "git"), 0o755);
	await chmod(join(bin, "docker"), 0o755);
	await chmod(join(bin, "gh"), 0o755);
	const sourceSha = "a".repeat(40);
	const digest = `sha256:${"b".repeat(64)}`;
	const summary = join(directory, "published image.md");
	const dockerCalls = join(directory, "docker-calls");
	const gitCalls = join(directory, "git-calls");
	const output = join(directory, "step-output");
	const result = spawnSync("bash", ["-c", script], {
		env: {
			...process.env,
			PATH: `${bin}:${process.env.PATH ?? ""}`,
			IMAGE_NAME: image,
			SOURCE_SHA: sourceSha,
			DIGEST: digest,
			GH_TOKEN: "",
			DOCKER_CALLS: dockerCalls,
			GIT_CALLS: gitCalls,
			GIT_ATTEMPTED: join(directory, "git-attempted"),
			DOCKER_ATTEMPTED: join(directory, "docker-attempted"),
			DOCKER_FAIL_ONCE: "0",
			NEXT_MAIN_RESPONSE: "",
			LIVE_MAIN_STATUS: "0",
			LIVE_MAIN_RESPONSE: `${sourceSha}\trefs/heads/main`,
			ALIASES_UPDATED: "true",
			LIVE_MAIN_SHA: sourceSha,
			GITHUB_OUTPUT: output,
			GITHUB_STEP_SUMMARY: summary,
			...overrides,
		},
		encoding: "utf8",
		timeout: 5_000,
	});
	assert.equal(result.error, undefined);
	assert.equal(result.signal, null);
	return { result, sourceSha, digest, summary, dockerCalls, gitCalls, output };
}

test("GHCR publisher preserves image publication without Platform deployment dispatch", async () => {
	const { source, job } = await readGhcrPublisher();
	assert.doesNotMatch(source, /RUNTIME_IMAGE_SYNC_TOKEN|k8s|maestro_runtime_image_published|catch-up/iu);
	assert.deepEqual(job.permissions, { contents: "read", packages: "write" });
	const metadata = job.steps.find((step) => step.id === "meta");
	const build = job.steps.find((step) => step.id === "push-sha");
	assert.equal(metadata.with.images, "${{ env.IMAGE_NAME }}");
	assert.equal(build.with.tags, "${{ env.IMAGE_NAME }}:sha-${{ github.sha }}");
	assert.equal(build.with.push, "true");
	assert.equal(build.with.platforms, "linux/amd64");
	const retag = job.steps.findIndex((step) => step.name === "Retag main and latest from the immutable digest");
	assert.ok(retag > job.steps.indexOf(build));
	assert.equal(job.steps[retag].id, "retag");
	assert.equal(job.steps.at(-1).env.ALIASES_UPDATED, "${{ steps.retag.outputs.aliases_updated }}");
	assert.equal(job.steps.at(-1).env.LIVE_MAIN_SHA, "${{ steps.retag.outputs.live_main_sha }}");
	assert.equal(job.steps.at(-1).name, "Summarize published image");
	assert.equal(job.steps.at(-1).condition, "");
	assert.equal(job.steps.at(-1).continueOnError, "");
});

test("GHCR publication summary records the actual immutable tag and digest", async (t) => {
	const { job, image } = await readGhcrPublisher();
	const receipt = await runGhcrShellStep(t, job.steps.at(-1).run, image);
	assert.equal(receipt.result.status, 0, receipt.result.stderr);
	assert.ok(existsSync(receipt.summary), "successful publication must write its receipt");
	const summary = await readFile(receipt.summary, "utf8");
	assert.ok(summary.includes(`${image}:sha-${receipt.sourceSha}`));
	assert.ok(summary.includes(`${image}@${receipt.digest}`));
	for (const tag of ["main", "latest", `sha-${receipt.sourceSha.slice(0, 7)}`]) {
		assert.ok(summary.includes(tag));
	}
	assert.doesNotMatch(summary, /deploy|k8s|catch-up/iu);
});

for (const missing of ["IMAGE_NAME", "SOURCE_SHA", "DIGEST"]) {
	test(`GHCR publication summary refuses missing ${missing}`, async (t) => {
		const { job, image } = await readGhcrPublisher();
		const receipt = await runGhcrShellStep(t, job.steps.at(-1).run, image, { [missing]: "" });
		assert.notEqual(receipt.result.status, 0, "missing publication identity must fail");
		assert.equal(existsSync(receipt.summary), false);
	});
}

test("GHCR retagging uses the pushed digest for main, latest and short SHA", async (t) => {
	const { job, image } = await readGhcrPublisher();
	const step = job.steps.find((step) => step.name === "Retag main and latest from the immutable digest");
	const receipt = await runGhcrShellStep(t, step.run, image);
	assert.equal(receipt.result.status, 0, receipt.result.stderr);
	assert.match(await readFile(receipt.output, "utf8"), /^aliases_updated=true$/mu);
	assert.deepEqual((await readFile(receipt.gitCalls, "utf8")).trim().split("\n"), [
		"-c", "http.lowSpeedLimit=1000", "-c", "http.lowSpeedTime=20",
		"ls-remote", "--exit-code", "origin", "refs/heads/main",
	]);
	assert.deepEqual((await readFile(receipt.dockerCalls, "utf8")).trim().split("\n"), [
		"buildx", "imagetools", "create",
		"--tag", `${image}:main`,
		"--tag", `${image}:latest`,
		"--tag", `${image}:sha-${receipt.sourceSha.slice(0, 7)}`,
		`${image}@${receipt.digest}`,
	]);
});

test("GHCR retagging skips aliases for a stale source while retaining its immutable receipt", async (t) => {
	const { job, image } = await readGhcrPublisher();
	const liveMain = "c".repeat(40);
	const step = job.steps.find((step) => step.name === "Retag main and latest from the immutable digest");
	const receipt = await runGhcrShellStep(t, step.run, image, {
		LIVE_MAIN_RESPONSE: `${liveMain}\trefs/heads/main`,
	});
	assert.equal(receipt.result.status, 0, receipt.result.stderr);
	assert.equal(existsSync(receipt.dockerCalls), false, "stale runs must not mutate aliases");
	assert.match(await readFile(receipt.output, "utf8"), /^aliases_updated=false$/mu);
	assert.match(await readFile(receipt.output, "utf8"), new RegExp(`^live_main_sha=${liveMain}$`, "mu"));
	const summaryReceipt = await runGhcrShellStep(t, job.steps.at(-1).run, image, {
		ALIASES_UPDATED: "false", LIVE_MAIN_SHA: liveMain,
	});
	assert.equal(summaryReceipt.result.status, 0, summaryReceipt.result.stderr);
	const summary = await readFile(summaryReceipt.summary, "utf8");
	assert.ok(summary.includes(`${image}:sha-${receipt.sourceSha}`));
	assert.ok(summary.includes(`${image}@${receipt.digest}`));
	assert.match(summary, /Skipped tags: main, latest/u);
	assert.ok(summary.includes(liveMain));
	assert.doesNotMatch(summary, /Updated tags/u);
});

test("GHCR retagging rechecks main before retrying a failed alias mutation", async (t) => {
	const { job, image } = await readGhcrPublisher();
	const step = job.steps.find((step) => step.id === "retag");
	const liveMain = "c".repeat(40);
	const receipt = await runGhcrShellStep(t, step.run, image, {
		DOCKER_FAIL_ONCE: "1",
		NEXT_MAIN_RESPONSE: `${liveMain}\trefs/heads/main`,
	});
	assert.equal(receipt.result.status, 0, receipt.result.stderr);
	assert.match(await readFile(receipt.output, "utf8"), /^aliases_updated=false$/mu);
	assert.doesNotMatch(await readFile(receipt.output, "utf8"), /^aliases_updated=true$/mu);
	assert.ok(receipt.result.stdout.includes(liveMain));
});

for (const [label, overrides] of [
	["remote HTTP failure", { LIVE_MAIN_STATUS: "128" }],
	["malformed pushed digest", { DIGEST: "sha256:bad" }],
	["empty main response", { LIVE_MAIN_RESPONSE: "" }],
	["malformed main SHA", { LIVE_MAIN_RESPONSE: "not-a-sha\trefs/heads/main" }],
	["wrong branch", { LIVE_MAIN_RESPONSE: `${"a".repeat(40)}\trefs/heads/other` }],
	["multiple refs", { LIVE_MAIN_RESPONSE: `${"a".repeat(40)}\trefs/heads/main\n${"c".repeat(40)}\trefs/heads/main` }],
]) {
	test(`GHCR retagging fails closed on ${label}`, async (t) => {
		const { job, image } = await readGhcrPublisher();
		const step = job.steps.find((step) => step.name === "Retag main and latest from the immutable digest");
		const receipt = await runGhcrShellStep(t, step.run, image, overrides);
		assert.notEqual(receipt.result.status, 0);
		assert.equal(existsSync(receipt.dockerCalls), false);
	});
}

test("GHCR retagging refuses an absent pushed digest before invoking Docker", async (t) => {
	const { job, image } = await readGhcrPublisher();
	const step = job.steps.find((step) => step.name === "Retag main and latest from the immutable digest");
	const receipt = await runGhcrShellStep(t, step.run, image, { DIGEST: "" });
	assert.notEqual(receipt.result.status, 0);
	assert.equal(existsSync(receipt.dockerCalls), false);
});
for (const replacement of ["echo skipped", 'if false; then node scripts/verify-staged-release.mjs release-binaries "$RELEASE_VERSION"; fi']) {
 test(`rejects bypassed authentication: ${replacement}`, () => {
  assert.ok(validateReleaseWorkflow(completeWorkflow.replace('node scripts/verify-staged-release.mjs release-binaries "$RELEASE_VERSION"', replacement)).some(f => f.includes("must be authenticated")));
 });
}

 test("rejects a token that cannot read unpublished signed releases", () => {
	const unreadable = completeWorkflow.replace("  binaries:\n    needs: prepare\n    permissions:\n      contents: write\n", "  binaries:\n    needs: prepare\n    permissions:\n      contents: read\n");
	assert.notEqual(unreadable, completeWorkflow);
	assert.ok(validateReleaseWorkflow(unreadable).some(f => f.includes("binaries permissions")));
});

for (const [before, after] of [
 ["    needs: [prepare, binaries, identity-readiness]", "    needs: [prepare, binaries]"],
 ["node .github/workflows/check-release-identity.mjs", "echo skipped"],
 ["    needs: [prepare, binaries, publish, post-publish-canary]", "    needs: [prepare, binaries, publish]"],
]) {
 test(`rejects missing release readiness: ${before}`, () => {
  const changed = completeWorkflow.replace(before, after);
  assert.notEqual(changed, completeWorkflow);
  assert.ok(validateReleaseWorkflow(changed).some(f => f.includes("Identity preflight") || f.includes("authenticated registry replay")));
 });
}


test("GHCR publication uses the dedicated Code image namespace", async () => {
  const workflow = await readFile(new URL("./ghcr-publish.yml", import.meta.url), "utf8");
  assert.match(workflow, /^  IMAGE_NAME: ghcr\.io\/dx-corp\/code$/mu);
  assert.doesNotMatch(workflow, /ghcr\.io\/dx-corp\/maestro/u);
  assert.ok(workflow.includes("images: ${{ env.IMAGE_NAME }}"));
  assert.ok(workflow.includes("tags: ${{ env.IMAGE_NAME }}:sha-${{ github.sha }}"));
});
