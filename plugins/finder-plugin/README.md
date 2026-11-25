# bmux_finder_plugin

Finder for tabs across workspaces: discovery over every shared tab. It
consumes tabs/workspace state and delegates fuzzy matching to bmux's
search-select prompt. Entries say whether a tab is in this attachment's
personal view, and those that are come first. Choosing a tab dispatches
`plugin:bmux.tabs:open-tab`, which adds it to the view when absent and switches
to it. `finder add-to-view` lists the tabs the view lacks and adds the chosen
one without switching (`plugin:bmux.tabs:add-tab-to-view`). A tab your view
labels shows its label, with the shared name in its detail; search matches
both.

`finder views` (`plugin:bmux.finder:views`, prefix `V`) is the tab view
picker: it lists your views (marking the current and the default one) and
switches this attachment to the chosen one (`plugin:bmux.tabs:switch-view`).
Its "New empty view…" and "New view from this one…" entries ask for a name,
then create the view and switch to it (`plugin:bmux.tabs:create-view`).

Defaults search every workspace, order by caller-local last visit before typing,
rank filtered results by relevance, and hide the current tab. Use
`filtered_sort_order = "inherit"` for strict recency even while filtering.
`wrap_selection = true` (default)
wraps Up/Down and Ctrl-P/Ctrl-N at the filtered list boundaries; set it to
`false`
to stop at either end. Home/End and page navigation keep their existing
behavior.
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
