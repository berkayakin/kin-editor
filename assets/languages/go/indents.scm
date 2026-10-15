[
  (block)
  (literal_value)
  (argument_list)
  (parameter_list)
  (type_parameter_list)
  (field_declaration_list)
  (interface_type)
  (import_spec_list)
  (var_spec_list)
] @indent

(const_declaration "(" @start) @indent
(type_declaration "(" @start) @indent

; gofmt keeps `case` level with `switch`; a case's statements go one deeper.
[
  (expression_case ":" @start)
  (type_case ":" @start)
  (communication_case ":" @start)
  (default_case ":" @start)
] @indent
