(function_definition
  body: (compound_statement (_)* @function.inside)) @function.around

; A ( … ) or [[ … ]] body; a { … } body is the pattern above.
(function_definition
  body: (_) @function.inside
  (#not-match? @function.inside "^\\{")) @function.around
