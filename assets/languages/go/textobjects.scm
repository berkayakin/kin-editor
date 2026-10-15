(function_declaration
  body: (block (statement_list)? @function.inside)) @function.around

(method_declaration
  body: (block (statement_list)? @function.inside)) @function.around

(func_literal
  body: (block (statement_list)? @function.inside)) @function.around

(method_elem) @function.around

(type_declaration
  (type_spec
    type: (struct_type
      (field_declaration_list (_)* @class.inside)))) @class.around

(type_declaration
  (type_spec
    type: (interface_type (_)* @class.inside))) @class.around
