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

Telete uses bundled libgit2 for repository operations. Configured workflow
commands can bring their own tools.

## Use

Every command selects a project JSON configuration. Submit the configured
repository's current `HEAD`, then inspect the queue:

```sh
telete --config /path/to/project/telete.json submit --follow
telete --config /path/to/project/telete.json status
```

Submission runs in the background. Ctrl-C stops following without cancelling
the job.

Deployment workflows default to [integrating the prepared commit into local
`main`](docs/queue-operate.md#integrate-before-deployment) before deploying it.
Configure a different local branch, use `telete merge JOB` for manual
integration, or disable integration per project.

By default, projects under the same macOS account share one execution slot.
Set `"queue": "repository"` in a project's config to let that repository run
alongside other projects, one job at a time. See the
[queue modes](docs/queue-operate.md#choose-a-queue-mode).

Optional [deployment batching](docs/queue-operate.md#batch-deployment-requests)
runs each change's CI, freezes already queued compatible requests into a group,
and combines their checked commits for one preparation and deployment.

## Further documentation

- [telete(1) manual](docs/telete.1): installation, commands, and options.
  View it with `man ./docs/telete.1`.
- [Operating contract](docs/queue-operate.md): configuration, execution, shared
  queues, and recovery.
- [Source layout](docs/source-layout.md): building, repository CI, and release
  versioning.
