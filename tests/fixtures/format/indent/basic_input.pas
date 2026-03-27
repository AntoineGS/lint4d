unit BasicIndent;

interface

type
TMyClass = class
private
FValue: Integer;
public
procedure DoSomething;
end;

implementation

procedure TMyClass.DoSomething;
var
x: Integer;
begin
x := 1;
if x > 0 then
begin
x := x + 1;
end;
end;

end.
