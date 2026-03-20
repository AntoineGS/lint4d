unit BadCasingMultiClass;

interface

type
  TClassA = class
  private
    FData: Integer;
  public
    procedure Run;
  end;

  TClassB = class
  private
    fData: string;
  public
    procedure Run;
  end;

implementation

procedure TClassA.Run;
begin
  fdata := 10;
end;

procedure TClassB.Run;
begin
  Fdata := 'test';
end;

end.
