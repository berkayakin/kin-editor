[
  (do_group)
  (else_clause)
  (case_item)
  (compound_statement)
  (subshell)
  (command_substitution)
  (array)
] @indent

; Not before `then` and `in`, which may start the next line.
(if_statement "then" @start) @indent
(elif_clause "then" @start) @indent
(case_statement "in" @start) @indent
