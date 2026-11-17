unit E2003GenericParams;
interface
implementation
type
  TBox<T> = class
    FValue: T;
    function Get: T;
  end;
function TBox<T>.Get: T;
begin
  Result := FValue;
end;
procedure P;
var B: TBox<Integer>; V: Integer; Bad: TBox<U>;
begin
  V := B.Get;
end;
end.
