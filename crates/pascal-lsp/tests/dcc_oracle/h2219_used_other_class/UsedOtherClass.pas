unit UsedOtherClass;

interface

type
  TA = class
  private
    FUnused: Integer;
    FUsed: Integer;
  end;

  TB = class
  public
    procedure Touch(A: TA);
  end;

implementation

procedure TB.Touch(A: TA);
begin
  A.FUsed := 1;
end;

end.
