unit UsesCommaBeforeDirective;

interface

uses A, B, {$I units.inc};

implementation

uses
  {$IFDEF X}
  D, {$I a.inc}, {$I b.inc};
  {$ELSE}
  C {$I c.inc};
  {$ENDIF}

end.
