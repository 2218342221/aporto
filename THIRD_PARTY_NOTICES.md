# Third-party notices

Aporto is licensed under [Apache-2.0](LICENSE). The following notices cover the
protocol definitions used by its AgentENV adapter and the fonts distributed with
the web client.

## AgentENV and E2B envd protocol

[`src/runtime/agentenv/wire.rs`](src/runtime/agentenv/wire.rs) is a handwritten Rust
compatibility subset of the envd process protocol. It retains the wire field
numbers, uses Aporto's own Rust type names, and omits messages and fields that the
adapter does not use.

The reference is AgentENV's
[`thirdparty/envd/proto/process.proto`](https://github.com/kvcache-ai/AgentENV/blob/34cdc8098096726646853a18cec7ae143995dcef/thirdparty/envd/proto/process.proto)
at commit `34cdc8098096726646853a18cec7ae143995dcef`. That version's
[envd README](https://github.com/kvcache-ai/AgentENV/blob/34cdc8098096726646853a18cec7ae143995dcef/thirdparty/envd/README.md)
identifies the upstream E2B envd project, now hosted at `e2b-dev/runtime`.

The protocol file is byte-for-byte identical to E2B's
[`packages/envd/spec/process/process.proto`](https://github.com/e2b-dev/runtime/blob/79fcdf59b093eddda444a76932a4d140850b71c6/packages/envd/spec/process/process.proto)
at commit `79fcdf59b093eddda444a76932a4d140850b71c6`; both have Git blob ID
`99376a0e378e0aa2e959dbbf00a73c36d88fadb4`. E2B publishes that source under
[Apache-2.0](https://github.com/e2b-dev/runtime/blob/79fcdf59b093eddda444a76932a4d140850b71c6/LICENSE).
The full Apache-2.0 license is included in Aporto's [LICENSE](LICENSE). The E2B
copyright and license notice is retained here:

```text
Copyright 2023 FoundryLabs, Inc.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
```

AgentENV's root [license](https://github.com/kvcache-ai/AgentENV/blob/34cdc8098096726646853a18cec7ae143995dcef/LICENSE)
is MIT. Its vendored `thirdparty/envd/` tree has no separate license or notice at
the referenced commit, so the E2B attribution above is retained alongside the
AgentENV notice below.

```text
MIT License

Copyright (c) 2026 AgentENV

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## Web fonts

The web client bundles unmodified font files from the following Fontsource
packages. Their original copyright notices and complete SIL Open Font License
1.1 texts are retained at the linked paths:

| Font | Package | License and copyright notice |
| --- | --- | --- |
| Google Sans Flex | `@fontsource-variable/google-sans-flex@5.3.1` | [OFL-1.1](apps/web/public/licenses/google-sans-flex.txt) |
| Noto Sans SC | `@fontsource-variable/noto-sans-sc@5.3.0` | [OFL-1.1](apps/web/public/licenses/noto-sans-sc.txt) |

These notices are included in the built web client's `/licenses/` directory.
See [Web fonts](docs/fonts.md) for upstream sources and distribution details.

## Design reference

[OpenAI Codex](https://github.com/openai/codex/tree/9b738582b13c2cdbeff54af0afd04c50c3e7ba09)
is a design reference for the execution interface and application-service
boundaries. The references are documented in [Design](docs/design.md) and
[Architecture](docs/architecture.md).
