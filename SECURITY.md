# Security policy

This software runs on volunteers' machines and executes work sent by a remote
gateway. That makes its trust boundary the whole point of the project, so
security reports are treated as first-class work rather than an interruption.

## Reporting a vulnerability

Email **security@kmplify.io** with `kmplify-node` in the subject, or use
GitHub's private vulnerability reporting on this repository. Please do not open
a public issue for anything that could be exploited against a running node.

Include what you would want to receive: what you did, what happened, what you
expected, and the version (`kmplify-node check` prints it).

We aim to acknowledge within 3 working days and to ship a fix or a mitigation
within 30 days for anything that lets a gateway exceed the boundaries below.
You will be credited in the release notes unless you would rather not be.

## What counts as a vulnerability here

The gateway is **not trusted**. It schedules work; it does not get to decide
what your hardware does. Anything that lets a gateway (or someone who has taken
one over) cross one of these lines is in scope:

- running an image the node has not pinned for that template
- running any container when `PROVIDER_WORKLOADS` is unset
- mounting a host path, or any volume outside `kmplify-fabric-*`
- escaping the container's dropped capabilities, PID or memory caps
- exceeding the operator's CPU, VRAM, RAM or disk ceilings
- reaching a port on the machine other than the loopback binding the node made
- keeping a session alive after SIGTERM, or after a `workload_stop`
- reading node credentials, or making the node reveal them
- causing the node to listen on any inbound port

Also in scope: anything that makes the node misreport what it is (advertising
VRAM or cores it does not have), since consumers schedule against those numbers.

## What does not count

- The gateway seeing which models you serve, your GPU model, and your declared
  country. That is what the node advertises by design, and the README says so.
- `PROVIDER_COUNTRY` being unverifiable. It is a stated preference, not an
  attestation, and it is documented as one.
- A malicious *consumer* prompt reaching your local model. Consumers send
  inference requests; that is the service. Prompt content is not a boundary the
  node claims to enforce.
- Resource use inside the ceilings you configured.
- Findings against a gateway you control, or against KMPLIFY's hosted services,
  which are outside this repository.

## Supported versions

The latest release, and `main`. There is no long-term support branch yet; when
there is, it will be listed here.
