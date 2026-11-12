unit BodyInInclude;

interface

type
  TFoo = class
    procedure Present;
  end;

implementation

{$I Body.inc}

end.
