unit W1000FieldAndProperty;

interface

type
  TDeprecatedMembers = class
  private
    FDeprecated: Integer deprecated;
    function GetValue: Integer;
  public
    property DeprecatedProperty: Integer read GetValue;
  end;

procedure UseDeprecatedMembers(Value: TDeprecatedMembers);

implementation

function TDeprecatedMembers.GetValue: Integer;
begin
  Result := FDeprecated;
end;

procedure UseDeprecatedMembers(Value: TDeprecatedMembers);
var
  N: Integer;
begin
  N := Value.DeprecatedProperty;
end;

end.
