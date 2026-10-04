unit UsesEndsInDirective;

interface

uses A {$I x.inc};

implementation

uses
  D,
  B,
  E
  {$I y.inc}
  // after the include
  ;

end.
