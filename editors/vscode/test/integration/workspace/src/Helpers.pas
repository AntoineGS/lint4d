unit Helpers;

interface

function Twice(Value: Integer): Integer;

implementation

function Twice(Value: Integer): Integer;
begin
  Result := Value * 2;
end;

end.
