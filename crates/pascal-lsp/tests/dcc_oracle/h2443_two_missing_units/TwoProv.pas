unit TwoProv;
interface
function UsesTwoUnits: Integer; inline;
implementation
uses FirstHelper, SecondHelper;
function UsesTwoUnits: Integer;
begin
  Result := FirstValue + SecondValue;
end;
end.
