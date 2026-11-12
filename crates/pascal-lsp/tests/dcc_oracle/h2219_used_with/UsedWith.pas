unit UsedWith;

interface

type
  TFoo = class
  private
    FUnused: Integer;
    FUsed: Integer;
  public
    procedure Touch;
  end;

implementation

procedure TFoo.Touch;
begin
  with Self do
    FUsed := 1;
end;

end.
