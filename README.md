# Telete

I often found myself waiting on other agents to finish their work or wrangling
Git worktrees, either myself or through another agent. I don't like worrying
about how to work while working, so I built Telete to manage the flow of changes
as I work with multiple agents in parallel.

## Install

Install the release binary and manual:

```sh
./ci/install.sh
```

Ensure `~/.local/bin` is on your `PATH`.

## Use

Every command selects a project JSON configuration. Submit the configured
repository's current `HEAD`, then inspect the queue:

```sh
telete --config /path/to/project/telete.json submit --follow
telete --config /path/to/project/telete.json status
```

Submission runs in the background. Ctrl-C stops following without cancelling
the job.

## Further documentation

- [telete(1) manual](docs/telete.1): installation, commands, and options.
  View it with `man ./docs/telete.1`.
- [Operating contract](docs/queue-operate.md): configuration, execution, shared
  queues, and recovery.
- [Source layout](docs/source-layout.md): building, repository CI, and release
  versioning.
