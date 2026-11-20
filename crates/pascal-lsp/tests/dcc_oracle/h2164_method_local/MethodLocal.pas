unit MethodLocal;

interface

type
  TFoo = class
    procedure Bar;
  end;

implementation

procedure TFoo.Bar;
var
  NeverUsed: Integer;
begin
end;

end.
