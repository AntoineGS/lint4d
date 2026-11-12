unit UsedInInclude;

interface

type
  TFoo = class
  private
    FUnused: Integer;
    FIncluded: Integer;
  public
    procedure Touch;
  end;

implementation

procedure TFoo.Touch;
begin
  {$I Use.inc}
end;

end.
