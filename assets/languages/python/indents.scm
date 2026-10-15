[
  (argument_list)
  (parameters)
  (list)
  (dictionary)
  (set)
  (tuple)
  (parenthesized_expression)
  (list_comprehension)
  (dictionary_comprehension)
  (set_comprehension)
  (generator_expression)
] @indent

(import_from_statement "(" @start) @indent

; A block after the `:` that opens it, comments after it or not.
[
  (function_definition ":" @start)
  (class_definition ":" @start)
  (if_statement ":" @start)
  (elif_clause ":" @start)
  (else_clause ":" @start)
  (for_statement ":" @start)
  (while_statement ":" @start)
  (try_statement ":" @start)
  (except_clause ":" @start)
  (finally_clause ":" @start)
  (with_statement ":" @start)
  (match_statement ":" @start)
  (case_clause ":" @start)
] @indent
