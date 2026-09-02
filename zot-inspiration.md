Two things worth stealing, though. SubBlobs as an explicit DAG with a BFS-and-visited-set for size is a cleaner answer to multi-arch double-counting than your current parent.is_none() heuristic in get_stats — it handles
  nesting to arbitrary depth and dedups shared layers within a repo, which your sum currently doesn't. And the FastRestartStamp pattern is a good idea for any index that's derived from an authoritative store: cheap proof that a
  rebuild can be skipped.
