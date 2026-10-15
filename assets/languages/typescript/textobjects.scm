(function_declaration
  body: (statement_block (_)* @function.inside)) @function.around

(function_expression
  body: (statement_block (_)* @function.inside)) @function.around

(generator_function_declaration
  body: (statement_block (_)* @function.inside)) @function.around

(generator_function
  body: (statement_block (_)* @function.inside)) @function.around

(method_definition
  body: (statement_block (_)* @function.inside)) @function.around

(arrow_function
  body: (statement_block (_)* @function.inside)) @function.around

; An expression body; a block body is the pattern above.
(arrow_function
  body: (_) @function.inside
  (#not-match? @function.inside "^\\{")) @function.around

(class_declaration
  body: (class_body (_)* @class.inside)) @class.around

(class
  body: (class_body (_)* @class.inside)) @class.around

(function_signature) @function.around

(method_signature) @function.around

(abstract_method_signature) @function.around

(abstract_class_declaration
  body: (class_body (_)* @class.inside)) @class.around

(interface_declaration
  body: (interface_body (_)* @class.inside)) @class.around

(enum_declaration
  body: (enum_body (_)* @class.inside)) @class.around

(type_alias_declaration
  value: (object_type (_)* @class.inside)) @class.around

(internal_module
  body: (statement_block (_)* @class.inside)) @class.around

(module
  body: (statement_block (_)* @class.inside)) @class.around
