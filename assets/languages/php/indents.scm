[
  (compound_statement)
  (declaration_list)
  (enum_declaration_list)
  (colon_block)
  (switch_block)
  (case_statement)
  (default_statement)
  (match_block)
  (arguments)
  (formal_parameters)
  (array_creation_expression)
  (namespace_use_group)
  (use_list)
] @indent

(match_conditional_expression "=>" @start) @indent
(match_default_expression "=>" @start) @indent
(arrow_function "=>" @start) @indent
