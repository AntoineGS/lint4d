unit ShadowedByLocal;

interface

type
  TFoo = class
  private
    FValue: Integer;
  public
    procedure Touch;
  end;

implementation

procedure TFoo.Touch;
var
  FValue: Integer;
begin
  FValue := 1;
end;

end.
