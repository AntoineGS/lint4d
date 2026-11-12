unit UsedViaProperty;

interface

type
  TFoo = class
  private
    FUnused: Integer;
    FValue: Integer;
  public
    property Value: Integer read FValue write FValue;
    procedure Touch;
  end;

implementation

procedure TFoo.Touch;
begin
  Value := Value + 1;
end;

end.
