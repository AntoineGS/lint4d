unit E2003PropertySpecifiers;
interface
implementation
type
  TProps = class
  private
    function GetB: Integer;
    procedure SetB(Value: Integer);
  public
    property A: Integer read FMissing;
    property B: Integer read GetB write SetMissing;
  end;
function TProps.GetB: Integer;
begin
  Result := 0;
end;
procedure TProps.SetB(Value: Integer);
begin
end;
end.
