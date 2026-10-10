# Fork scope

This fork targets Symphony integration rather than parity with every upstream Zeron agent. Symphony is the only production runtime. Legacy native drivers, ACP adapters, vendor installers and their test/maintenance jobs are intentionally removed.

The retained workspace includes chat/transcript rendering, provider/model and command selection, tool outputs/diffs, child-agent views, terminals, browser previews, local documents and optional device sync. Shared mobile and edge components are retained where they support these services.

For supported setup and known integration limits, use [docs/symphony.md](symphony.md). Historical performance reports and upstream design notes do not define this fork's supported runtime catalog.
