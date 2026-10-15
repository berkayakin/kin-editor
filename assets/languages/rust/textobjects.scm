(function_item
  body: (block (_)* @function.inside)) @function.around

(function_signature_item) @function.around

(closure_expression
  body: (block (_)* @function.inside)) @function.around

; An expression body; a block body is the pattern above.
(closure_expression
  body: (_) @function.inside
  (#not-match? @function.inside "^\\{")) @function.around

(struct_item
  body: (field_declaration_list (_)* @class.inside)) @class.around

(struct_item
  body: (ordered_field_declaration_list)) @class.around

(enum_item
  body: (enum_variant_list (_)* @class.inside)) @class.around

(union_item
  body: (field_declaration_list (_)* @class.inside)) @class.around

(trait_item
  body: (declaration_list (_)* @class.inside)) @class.around

(impl_item
  body: (declaration_list (_)* @class.inside)) @class.around

(mod_item
  body: (declaration_list (_)* @class.inside)) @class.around
