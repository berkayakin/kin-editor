(function_definition
  body: (compound_statement (_)* @function.inside)) @function.around

(method_declaration
  body: (compound_statement (_)* @function.inside)) @function.around

; Abstract and interface methods.
(method_declaration !body) @function.around

(anonymous_function
  body: (compound_statement (_)* @function.inside)) @function.around

(arrow_function
  body: (_) @function.inside) @function.around

(class_declaration
  body: (declaration_list (_)* @class.inside)) @class.around

(anonymous_class
  body: (declaration_list (_)* @class.inside)) @class.around

(interface_declaration
  body: (declaration_list (_)* @class.inside)) @class.around

(trait_declaration
  body: (declaration_list (_)* @class.inside)) @class.around

(enum_declaration
  body: (enum_declaration_list (_)* @class.inside)) @class.around
