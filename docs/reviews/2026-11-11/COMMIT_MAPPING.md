# Local-to-published commit mapping

Base main: `e31f3626ce06726cf8cfe654e6699b1d5c3b1401`.
Published implementation head: `fe2d68239c0341b39e97e98705333ec798d19164`.
Original reviewed local head: `2acdc67756b5b98894fac3e05ab0e87283b4a577`.

| Original local commit | Published implementation commit | Identical tree SHA |
| --- | --- | --- |
| `8189903f45ba9002e1bdcdf83eedc4eca3e10486` | [`21ef79289137f1a20951ec62f76f0b42d50da21a`](https://github.com/Mischa-dev/harness/commit/21ef79289137f1a20951ec62f76f0b42d50da21a) | `82715008269bcc244add3fda25fdb877457205fb` |
| `5c485e9a233cc6c00b99535b1cbd3d296d5b6ae8` | [`e034565fd57f261b5b741bc9881d1a0c21b3f7d0`](https://github.com/Mischa-dev/harness/commit/e034565fd57f261b5b741bc9881d1a0c21b3f7d0) | `e1752c5a838fa255fc1d162534a81f54d265dad8` |
| `2acdc67756b5b98894fac3e05ab0e87283b4a577` | [`fe2d68239c0341b39e97e98705333ec798d19164`](https://github.com/Mischa-dev/harness/commit/fe2d68239c0341b39e97e98705333ec798d19164) | `dcb4718feee38120aed35201436a4802287736cd` |

The connected GitHub API created new commit IDs because author/committer metadata differed from the original local commits. Each uploaded tree SHA matched its original local counterpart. The published branch was read through the connector and independently fetched with Git. Its 11 changed implementation/test/fixture paths matched the reviewed change set, and a complete tree diff against the tested local head was empty.

The separately delivered Git bundle and patch series retain the original local commits. Both were imported in a separate clean checkout and produced the same final implementation tree. They are optional offline copies; review from this branch directly using [the handoff](CLAUDE_HANDOFF.md).

The documentation commit is a direct child of the published implementation head and adds only the four Markdown files in this directory. Its tree intentionally differs by these review documents. No implementation content is changed, and the original portable package remains intact.
