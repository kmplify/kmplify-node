# Trademarks

The code in this repository is Apache-2.0. The name on it is not.

That is not a loophole, it is how the licence is written: Apache-2.0 section 6
grants patent and copyright rights and explicitly grants no trademark rights.
This file says plainly what that means here, so nobody has to guess.

## What this file does not do

Nothing below restricts what the licence gives you. You may use, study,
modify, redistribute and sell this software, including commercially, including
as part of a competing product, without asking anyone. Forking is a right, not
a favour, and no permission is needed for any of it.

This file is about names and logos only.

## The marks

Used by KMPLIFY, whether or not registered in a given jurisdiction:

- **KMPLIFY**, as a word
- the KMPLIFY logo and wordmark
- **kmplify-node**, as the name of this project
- **KMPLIFY Compute Fabric** and **KMPLIFY GPU Fabric**, as names for the
  network this agent connects to

## What you may do without asking

Truthful references do not need permission, and we will not chase them:

- Saying your software works with, connects to, or is compatible with KMPLIFY
  or the KMPLIFY Compute Fabric.
- Saying your project is a fork of, is based on, or is derived from
  kmplify-node.
- Naming the project in articles, talks, tutorials, benchmarks, reviews and
  comparisons, including unflattering ones.
- Redistributing this software **unmodified** under its own name, for example
  packaging it for a distribution or a registry.
- Keeping the `NOTICE` file, copyright headers and attribution intact. The
  licence requires that (section 4(d)) and doing it is never a trademark
  problem, including in a renamed fork. See below.

## What needs written permission

- Naming a **modified** version kmplify-node, or naming any product, service,
  company, domain, app store listing, package or social account with KMPLIFY
  in it.
- Using the logo or wordmark, in any product, site or material that is not
  ours. Logos are more restricted than words because they are the part people
  recognise at a glance.
- Anything that suggests endorsement, affiliation, partnership, certification
  or official status that does not exist.
- Merchandise.

## Forks and modified versions

Fork freely. If you change the code and distribute it, give it your own name:

1. Rename the project, the binary, the crate and the service unit.
2. Remove the KMPLIFY logo and wordmark from your materials.
3. **Keep `NOTICE`, `LICENSE` and the copyright headers.** Apache-2.0 requires
   this and it does not conflict with anything above. Attribution is not
   branding.
4. Say what it is: "a fork of kmplify-node" is accurate, welcome, and needs no
   permission. "kmplify-node, patched" as a product name is not.

## Why the line sits here

The obvious reason is the ordinary one: a name is how people find their way
back to us, and giving away the code was never a decision to give away the
company.

The less obvious reason matters more, and it is a security property rather
than a marketing one. This agent runs containers on other people's hardware
under controls that only work if they are actually present: image pinning,
per-template consent, provider-set ceilings. An operator who installs
"kmplify-node" is relying on those controls being the ones we ship and
maintain. If anyone could attach the name to a modified build, that reliance
would be worth nothing, and the audit trail that makes the fabric trustworthy
would end at a name that means whatever the last packager decided.

So the code stays open because you should be able to read what runs on your
machine, and the name stays ours so that reading it tells you something.

## Asking

Permission is usually a short conversation, and the answer is often yes for
things that help the ecosystem. Write to hallo@kmplify.de and describe the use.

If you believe something here is being used in a way that misleads people, the
same address works, and so does opening an issue.
