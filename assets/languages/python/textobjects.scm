(function_definition
  body: (block) @function.inside) @function.around

(decorated_definition
  definition: (function_definition
    body: (block) @function.inside)) @function.around

(lambda
  body: (_) @function.inside) @function.around

(class_definition
  body: (block) @class.inside) @class.around

(decorated_definition
  definition: (class_definition
    body: (block) @class.inside)) @class.around
