# MegaMan interface

Use a dense, file-centered interface with intrinsic rows and columns, clear focus and few decorative elements. Keep filenames, location, selection and operation state easy to scan.

The shared palette uses ground `#191d1e`, chrome `#222728`, dividers `#343b3e`, primary text `#e4e8e8`, secondary text `#a4afb2`, selected rows `#087d99`, white selected text and cyan unselected focus. Selected cursor outlines remain white. File rows are 25 logical pixels with bounded overscan.

Controls size from content and available space. Secondary details may move to tooltips in short panes; navigation, errors, cancellation and acknowledgement must remain reachable. A zero-height viewport still updates measured width. Build only visible file/receipt rows; retain interaction state outside transient UI builds.

Use the existing embedded font and native fallback support. Multilingual shaping, IME and accessibility require explicit qualification. No new branding assets are required for the application shell.
