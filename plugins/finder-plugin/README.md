# bmux_finder_plugin

Standalone read-only finder for tabs across workspaces. It consumes retained
tabs/workspace state and delegates fuzzy matching to bmux's search-select
prompt.

Defaults search every workspace, order by caller-local last visit, preserve that
order while filtering, and hide the current tab. `wrap_selection = true` (default)
wraps Up/Down and Ctrl-P/Ctrl-N at the filtered list boundaries; set it to `false`
to stop at either end. Home/End and page navigation keep their existing behavior.
Settings also support
workspace/tab arrangement order, alphabetical labels, relevance ranking, and
placing the current tab last or in normal order. See
[`Workspace and Finder Settings`](../../docs/concepts.md#workspace-and-finder-settings)
for the configuration reference. Visit history is transient per client; another
attachment's navigation does not reorder your finder.

The finder prefers a 90-column modal, with content-driven height capped at 30
rows including the search field and footer. Smaller terminals constrain the
modal to the available space. Filtering preserves its height; resizing the
terminal recalculates the bounds without resetting the search or selection.
