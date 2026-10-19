# Browser core

Worker-owned directory enumeration publishes immutable native-path catalogs with opaque revisions. Selection uses normalized ranges; Select All does not allocate one value per row. Every index-bearing action checks its listing revision. Snapshot tickets materialize operation paths on the retained worker, and old catalogs are reclaimed there after external leases release.

Catalogs and retained revisions can grow with item count; fragmented selections cost O(r) ranges. A blocked filesystem call delays cooperative cancellation. Native identities, not display strings, own operation targeting.
