# Summ Container Registry

Summ is currently a research and engineering project. I want to create an OCI/docker container registry that is:
- Easy to use - get up and running very quickly, requires minimal configuration, with sensible defaults.
- Practical utilities - web ui to visualise repository size, manifests and tags count, manifest <-> layer blob bidirectional map, artifact pull statistics, tag change history. In other container registry products, these features are often missing or an after thought, Summ provide first-class support to those features.
- Created for AI Agent to operate - most of the time we let AI agents to push, pull, purge, and reason about images and artifacts stats, so Summ will be shipped with MCP and SKILLS for agents.
- Extremely efficient - using bespoke data structure to store registry specific data entities instead of using a general purpose. database, when information can be encoded in less bytes, the system becomes much much faster.
- Designed to take advantage of mordern computer hardware - Watch [Cliff Click's talk](https://www.youtube.com/watch?v=OFgxAFdxYAQ).
