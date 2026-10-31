# rovacom

A Rust port of [novacom](https://github.com/openwebos/novacom), the host client for talking to webOS devices (such as the Palm Pre3) through a novacom daemon. rovacom keeps the C client's command line, output and wire protocol, works with `novacomd` and `rovacomd`, and also runs under the name `novacom`. Linux and macOS are both supported.

- [Build, install and run](docs/DEPLOY.md)
- [Compatibility matrix and test report](docs/STATUS.md)
- [Assessment of the C original](docs/ASSESSMENT.md) and [improvement log](docs/IMPROVEMENTS.md)

Licensed under Apache-2.0; see [LICENSE](LICENSE) and [NOTICE](NOTICE).

## Repository CI baseline

Baseline GitHub Actions used by this repository:

- Adhere to least privilege principles for workflow permissions
- Use of commit hashes for pinning GitHub Actions dependencies
- Use of Dependabot to update commit hashes as necessary
- Use of [andrej-karpathy-skills](https://github.com/forrestchang/andrej-karpathy-skills/blob/main/CLAUDE.md) as CLAUDE.md starting point
- Use of OpenSSF's [Security Scorecard](https://github.com/ossf/scorecard) (SCORECARD_TOKEN setup required)
- Use of Step Security's [Harden Runner](https://github.com/step-security/harden-runner)
- Codespell
- Super-Linter
- Semgrep CE
- (optional) Sync commits to GitLab (GITLAB_TOKEN setup required)
  - Create PAT on GitLab with API, repo read and repo write permissions
  - Add PAT into GitHub repo as a secret named GITLAB_TOKEN
