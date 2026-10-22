# Release Notes

## v0.5.0 - 2026-10-22

This release adds scoped, durable index purges (index_forget MCP tool and bite index forget CLI), an opt-in skip of the exact-total count scan, and safer helper-death replay semantics. It also enforces data-dir privacy at startup, resolves kill from absolute paths, dispatches requests concurrently, and updates dependencies including rustls-webpki for RUSTSEC-2026-0382.

### Commits
- [`8385e18`](https://github.com/Ktesio/bite-mcp/commit/8385e1874f74c8fa2c165103e315b1eb992b3789) fix(mcp): never blind-replay mutating tools after a helper death
- [`e0d8cc6`](https://github.com/Ktesio/bite-mcp/commit/e0d8cc63a95207c1f54bc1d8d4143ed43856cdca) Merge pull request #39 from Ktesio/feat/replay-safe-tools
- [`f2f9ac7`](https://github.com/Ktesio/bite-mcp/commit/f2f9ac7a2f2c71095bfc513582b83ab1e153e5ea) perf(index): opt-in skip of the exact-total count scan
- [`1fe8fbf`](https://github.com/Ktesio/bite-mcp/commit/1fe8fbf052d1cda735a0782b0a1f563f5fb9fe22) fix(bridge): split helper_exited_unsent so safe replays are not refused
- [`11176db`](https://github.com/Ktesio/bite-mcp/commit/11176db2a793559aec00f3f2b22acdf8ffa1010e) Merge pull request #40 from Ktesio/perf/skip-exact-count
- [`972b0e9`](https://github.com/Ktesio/bite-mcp/commit/972b0e9d0fe63db252bcb8d45737710659ac82e8) Merge pull request #41 from Ktesio/feat/unsent-replay-split
- [`2e248ea`](https://github.com/Ktesio/bite-mcp/commit/2e248ea24c405568cb3c6e72ea0abd061f4b8c41) feat(index): scoped purge of local index rows via index_forget
- [`fcb8bc5`](https://github.com/Ktesio/bite-mcp/commit/fcb8bc590cf11654529ab6dfc630072f36ba3063) perf(index): scope the global index mutex to the lazy open
- [`4bb5956`](https://github.com/Ktesio/bite-mcp/commit/4bb5956c4b48d5857d099d7091c21f842d49464c) fix(index): hold the store writer lock across index_forget's delete
- [`3d6f1da`](https://github.com/Ktesio/bite-mcp/commit/3d6f1daa693380c76cef5aa50b0ad12cf0be81b5) Merge pull request #44 from Ktesio/feat/index-purge
- [`13da74f`](https://github.com/Ktesio/bite-mcp/commit/13da74fa7ac2e3fbd5403a68f2446d01d026585b) chore(deps): bump the lance-arrow-stack group with 3 updates (#42)
- [`30e8ba1`](https://github.com/Ktesio/bite-mcp/commit/30e8ba1456513589519e08a30fa7d5069f692566) chore(deps): bump the cargo-lockfile-only group with 2 updates (#43)
- [`9435eee`](https://github.com/Ktesio/bite-mcp/commit/9435eee2ab072ab4148468a1c85777e903e51a3b) Merge remote-tracking branch 'origin/main' into perf/index-lock-scope
- [`727cc02`](https://github.com/Ktesio/bite-mcp/commit/727cc02184c13379e79a2b62407f3551f4bc32f8) security(index): resolve kill from absolute paths, fail safe when no probe runs
- [`d081a10`](https://github.com/Ktesio/bite-mcp/commit/d081a1040c3d30f97b0175f658e93c54acfec773) perf(index): scope the global index mutex to the lazy open (#45)
- [`51428e6`](https://github.com/Ktesio/bite-mcp/commit/51428e6d4c5d4d9f174d752af2e8a0a5fa26b2b1) Merge pull request #46 from Ktesio/security/absolute-kill-path
- [`fcc7a5e`](https://github.com/Ktesio/bite-mcp/commit/fcc7a5e8f214b0c9196b94b31eb0b6a80b2c6df8) feat(index): declare the no_body param on index_rebuild and add the --no-body CLI flag
- [`eb31ac8`](https://github.com/Ktesio/bite-mcp/commit/eb31ac81c05501d87ac50e724e27eddc2725ba53) chore(deps): bump rustls-webpki to 0.103.16 for RUSTSEC-2026-0382
- [`dd3beb0`](https://github.com/Ktesio/bite-mcp/commit/dd3beb06a3937c8ca1fa917f88671d7005170a20) feat(cli): add 'bite index forget' subcommand for scoped purges
- [`6189a1e`](https://github.com/Ktesio/bite-mcp/commit/6189a1ea748fd547b785a4c3b43834fcb051f939) chore(deps): bump rustls-webpki to 0.103.16 for RUSTSEC-2026-0382
- [`d247723`](https://github.com/Ktesio/bite-mcp/commit/d2477237ea8af86b57eb593c72046f49f4008234) Merge pull request #47 from Ktesio/feat/index-rebuild-no-body
- [`38caacf`](https://github.com/Ktesio/bite-mcp/commit/38caacf954c7b9a9ab924c95d7f1f4a2f7a90e60) Merge remote-tracking branch 'origin/main' into feat/index-forget-cli
- [`f1220e6`](https://github.com/Ktesio/bite-mcp/commit/f1220e63f3d98f07390e20487934a30016db4695) Merge pull request #48 from Ktesio/feat/index-forget-cli
- [`be48caf`](https://github.com/Ktesio/bite-mcp/commit/be48caf02648f022a55893bd74e2d3d948bcc7df) security(core): enforce data-dir privacy at every startup, cover batches/quarantine
- [`31dc858`](https://github.com/Ktesio/bite-mcp/commit/31dc858ee027577811aff6217cfcee2e2cf0b5de) perf(mcp): dispatch requests concurrently, narrow the bridge lock to spawn/clone
- [`5d9db52`](https://github.com/Ktesio/bite-mcp/commit/5d9db5241326c522222bb64c92accaf2c7534eed) Merge pull request #49 from Ktesio/security/data-dir-privacy-enforcement
- [`4a45aa8`](https://github.com/Ktesio/bite-mcp/commit/4a45aa84b02c752d2ff5bf971e71897d8f100ec1) Merge pull request #50 from Ktesio/perf/concurrent-request-dispatch
- [`92aa962`](https://github.com/Ktesio/bite-mcp/commit/92aa96207dc00096471383430ae3757a73d0d33d) feat(index): make index_forget purges durable across re-crawls
- [`7d90504`](https://github.com/Ktesio/bite-mcp/commit/7d90504cf5893c94a51bdd4248f9e6de6848020b) test(docs): fence docs/protocol.md against tool-registry drift
- [`5676c7d`](https://github.com/Ktesio/bite-mcp/commit/5676c7d12b1f7f876533ba98c7b1c14526f93192) Merge pull request #51 from Ktesio/feat/durable-purge-scopes
- [`fffa1aa`](https://github.com/Ktesio/bite-mcp/commit/fffa1aade09a2eb0855e36d2d8f34e251dcf799c) Merge branch 'main' into docs/protocol-registry-fence
- [`5f2a8a0`](https://github.com/Ktesio/bite-mcp/commit/5f2a8a08bb14dfe24687d3b130701ac29a5d8187) Merge pull request #52 from Ktesio/docs/protocol-registry-fence

