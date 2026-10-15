; (), [] and {} pair by the grammar's tokens without a query.
(string "\"" @open "\"" @close)
(string "'" @open "'" @close)
(template_string "`" @open "`" @close)
(template_substitution "${" @open "}" @close)
(jsx_opening_element "<" @open ">" @close)
(jsx_closing_element "</" @open ">" @close)
(jsx_self_closing_element "<" @open "/>" @close)
