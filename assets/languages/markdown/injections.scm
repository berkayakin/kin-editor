;; Paragraph, heading and table-cell text, where inline syntax (emphasis,
;; links, code spans) lives.
((inline) @injection.content (#set! injection.language "markdown_inline"))
((pipe_table_cell) @injection.content (#set! injection.language "markdown_inline"))
