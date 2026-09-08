# bmux_finder_plugin

Standalone read-only finder for tabs across workspaces. It consumes retained
windows/workspace state and delegates fuzzy matching to bmux's search-select
prompt.

The finder prefers a 90-column modal, with content-driven height capped at 30
rows including the search field and footer. Smaller terminals constrain the
modal to the available space. Filtering preserves its height; resizing the
terminal recalculates the bounds without resetting the search or selection.
