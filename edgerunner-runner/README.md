# LOB Edgerunner batch image

Consumes `EDGERUNNER_TASK_JSON` schema `dev.fpl.lob.run/v1` and deterministically
runs `spec -> schematic -> run -> board -> drc`. The inherited
`edgerunner-report` binary emits lifecycle and `dev.fpl.design-run-event/v1`
events. Every output file is recorded with its byte count and SHA-256 digest in
`.edgerunner/lob/artifacts.json`.

`min_confidence` defaults to `0.5`. Any decision below it emits a structured
`needs_human`/`needs_approval` report and stops before schematic generation.
When Edgerunner supplies a Bifrost lease, the runner maps its key and base URL
onto LOB's existing OODA client environment rather than adding another client.

Build from the repository root:

```sh
docker build --build-context ooda=../../rlcd/ooda \
  -f edgerunner-runner/Dockerfile -t lob-edgerunner:dev .
```

The named context calls the ecosystem-owned OODA crate instead of copying or
forking its decision client; it is required by LOB's existing workspace path.
