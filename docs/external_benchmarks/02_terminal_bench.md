# bench/PR5 — Terminal-Bench adapter (BLOCKED on Docker)

## Design sketch
Terminal-Bench tasks run in Docker with a tmux session; agents
integrate as an installed-agent (binary inside the container) or an
external adapter class. anie path: installed-agent — bake the anie
binary into the task container (volume mount; glibc base), point it
at the HOST Ollama via OLLAMA_HOST=host.docker.internal /
--network=host, run `anie --print` with the task instruction, let
the tb grader check the terminal state.

Prereqs: Docker engine (user install), then `pip install
terminal-bench` and `tb run --agent <adapter> --task-set core`.

## Exit criteria
- tb core subset score for anie (both modes) once Docker exists;
  adapter merged + smoke green before that behind a docker-detect
  skip.
