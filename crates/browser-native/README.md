# Native desktop services

The X11 clipboard runs on a dedicated worker with bounded direct/INCR reads, ownership tracking and byte-preserving native file lists. A queued request is not confirmed ownership. Peer changes, timeout and cancellation produce explicit outcomes.

The DnD module currently supplies lifecycle/admission/completion semantics. Actual native drag/drop transport and Wayland adapters remain in development. Native clipboard tests require the native-x11-tests feature, Xvfb and Python-Xlib; default API tests require no display.
